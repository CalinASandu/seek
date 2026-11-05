use super::*;
use crate::claims::on_fixed_drive;

const GUID: &str = "{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}";

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

fn env(name: &str) -> Option<String> {
    match name.to_ascii_lowercase().as_str() {
        "systemroot" | "windir" => Some(r"C:\Windows".into()),
        "seek_apps_test_dir" => Some(r"C:\Tools".into()),
        _ => None,
    }
}

/// `PROGRA~1` written out, as Windows would on a fixed drive.
fn expand(p: &str) -> String {
    p.replace("PROGRA~1", "Program Files")
}

fn with_ctx<R>(dir: &dyn Fn(&str) -> bool, file: &dyn Fn(&str) -> bool, f: impl FnOnce(&Ctx) -> R) -> R {
    f(&Ctx { env: &env, expand: &expand, dir_exists: dir, file_exists: file })
}

/// Everything is there.
fn all_there<R>(f: impl FnOnce(&Ctx) -> R) -> R {
    with_ctx(&|_| true, &|_| true, f)
}

fn build(entries: Vec<RawEntry>) -> Vec<AppRow> {
    all_there(|ctx| rows(entries, &places(), &[], ctx, &plus_two))
}

fn find(e: &RawEntry) -> Option<Found> {
    all_there(|ctx| find_folder(e, &places(), ctx, &[]))
}

fn folder(e: &RawEntry) -> Option<PathBuf> {
    find(e).map(|f| f.path)
}

fn parse(cmd: &str) -> Option<UninstallCommand> {
    all_there(|ctx| parse_uninstall_with(cmd, ctx))
}

fn exe(path: &str, args: &str) -> Option<UninstallCommand> {
    Some(UninstallCommand::Exe { path: path.into(), args: args.into() })
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
    let shown = build(vec![entry("Keep"), sys, child, no_cmd, nameless]);
    assert_eq!(names(&shown), ["Keep"]);
}

#[test]
fn a_row_with_only_a_quiet_uninstaller_is_hidden() {
    let mut quiet_only = entry("QuietOnly");
    quiet_only.uninstall_string.clear();
    quiet_only.quiet_uninstall_string = r#""C:\q\u.exe" /S"#.into();
    assert!(!is_listed(&quiet_only));
    assert!(build(vec![quiet_only]).is_empty());
}

#[test]
fn entry_by_id_only_gives_what_the_list_would_show() {
    // entry_by_id is `read_uninstall_entry(..).filter(is_listed)`; these are the cases it refuses.
    let mut seek = entry("Seek");
    seek.publisher = "Seek".into();
    seek.key = "Seek".into();
    let mut sys = entry("Sys");
    sys.system_component = true;
    let mut child = entry("Child");
    child.parent_key_name = "Parent".into();
    let mut quiet = entry("Quiet");
    quiet.uninstall_string.clear();
    quiet.quiet_uninstall_string = "x.exe /S".into();
    for hidden in [seek, sys, child, quiet] {
        assert!(!is_listed(&hidden), "{}", hidden.display_name);
    }
    assert!(is_listed(&entry("Fine")));
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

#[test]
fn a_row_whose_uninstaller_cant_be_started_stays_listed_with_a_note() {
    let mut e = entry("Odd");
    e.uninstall_string = "uninstall.exe".into();
    let shown = build(vec![e]);
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].note.as_deref(), Some("Seek can't start this app's uninstaller."));
    // With the runtime note too: both, joined.
    let mut rt = entry("Microsoft .NET Runtime 8");
    rt.uninstall_string = "/S".into();
    let shown = build(vec![rt]);
    assert_eq!(shown[0].note.as_deref(), Some("Other apps may need this. Seek can't start this app's uninstaller."));
    // A good command adds nothing.
    assert_eq!(build(vec![entry("Fine")])[0].note, None);
}

// ---- the folder -----------------------------------------------------------------------------------------

#[test]
fn install_location_wins() {
    let mut e = entry("App");
    e.install_location = r"D:\Games\App\".into();
    e.display_icon = r"C:\Program Files\Other\other.exe,0".into();
    let f = find(&e).unwrap();
    assert_eq!(f.path, PathBuf::from(r"D:\Games\App"));
    assert_eq!(f.source, FolderSource::InstallLocation);
}

#[test]
fn folder_falls_back_to_the_icon_then_the_uninstaller() {
    let mut e = entry("App");
    e.display_icon = r#""C:\Program Files\Acme App\app.exe",1"#.into();
    let f = find(&e).unwrap();
    assert_eq!((f.path, f.source), (PathBuf::from(r"C:\Program Files\Acme App"), FolderSource::Icon));
    e.display_icon.clear();
    let f = find(&e).unwrap();
    assert_eq!((f.path, f.source), (PathBuf::from(r"C:\Program Files\Acme"), FolderSource::Uninstaller));
    // An icon in a rejected place does not stop the uninstaller from being tried.
    e.display_icon = r"C:\Windows\Installer\{G}\icon.ico".into();
    assert_eq!(folder(&e), Some(PathBuf::from(r"C:\Program Files\Acme")));
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
        assert_eq!(folder(&e), None, "{icon}");
    }
    // The same rules hold for a registered InstallLocation that is a root.
    let mut e = entry("App");
    e.uninstall_string = format!("MsiExec.exe /X{GUID}");
    e.install_location = r"C:\Program Files".into();
    assert_eq!(folder(&e), None);
}

#[test]
fn roots_and_the_profile_with_its_well_known_folders_are_not_app_folders() {
    let bad = [
        r"C:\setup.exe",
        r"D:\unins.exe",
        r"\\nas\share\unins.exe",
        r"C:\Users\me\unins.exe",
        r"C:\Users\me\Downloads\setup.exe",
        r"C:\Users\me\Desktop\setup.exe",
        r"C:\Users\me\Documents\setup.exe",
        r"C:\Users\me\OneDrive\setup.exe",
        r"C:\Users\me\Saved Games\setup.exe",
    ];
    for exe in bad {
        let mut e = entry("App");
        e.uninstall_string = format!(r#""{exe}""#);
        assert_eq!(folder(&e), None, "{exe}");
        // Registered folders follow the same rule.
        let mut r = entry("Reg");
        r.install_location = exe.rsplit_once('\\').unwrap().0.to_string();
        r.uninstall_string = format!("MsiExec.exe /X{GUID}");
        assert_eq!(folder(&r), None, "{exe}");
    }
    // Other folders in the profile, and deeper ones, are fine, guessed or registered.
    for tool in [r"C:\Users\me\miniconda3", r"C:\Users\me\anaconda3", r"C:\Users\me\scoop", r"C:\Users\me\Tools\App"] {
        let mut e = entry("App");
        e.uninstall_string = format!(r#""{tool}\unins.exe""#);
        assert_eq!(folder(&e), Some(PathBuf::from(tool)), "{tool}");
        e.install_location = tool.to_string();
        assert_eq!(find(&e).map(|f| f.source), Some(FolderSource::InstallLocation), "{tool}");
    }
    let mut e = entry("App");
    e.uninstall_string = r#""\\nas\share\App\unins.exe""#.into();
    assert_eq!(folder(&e), Some(PathBuf::from(r"\\nas\share\App")));
}

#[test]
fn short_names_are_judged_and_returned_in_full() {
    let mut e = entry("App");
    e.uninstall_string = r#""C:\PROGRA~1\unins000.exe""#.into();
    assert_eq!(folder(&e), None, "that is the Program Files root");
    e.uninstall_string = r#""C:\PROGRA~1\Acme\unins000.exe""#.into();
    assert_eq!(folder(&e), Some(PathBuf::from(r"C:\Program Files\Acme")));
}

#[test]
fn a_stale_install_location_falls_through_to_the_icon_then_the_uninstaller() {
    let mut e = entry("App");
    e.install_location = r"D:\Gone\App".into();
    e.display_icon = r"C:\Program Files\Acme App\app.exe,0".into();
    let exists = |p: &str| !p.starts_with(r"D:\Gone");
    let f = with_ctx(&exists, &|_| true, |ctx| find_folder(&e, &places(), ctx, &[])).unwrap();
    assert_eq!((f.path, f.source), (PathBuf::from(r"C:\Program Files\Acme App"), FolderSource::Icon));
    // And the icon's folder gone too: the uninstaller's.
    let exists = |p: &str| p == r"C:\Program Files\Acme";
    let f = with_ctx(&exists, &|_| true, |ctx| find_folder(&e, &places(), ctx, &[])).unwrap();
    assert_eq!(f.source, FolderSource::Uninstaller);
    // Nothing exists: no folder, but the app is still listed.
    let rows = with_ctx(&|_| false, &|_| true, |ctx| rows(vec![e.clone()], &places(), &[], ctx, &plus_two));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].folder, None);
}

#[test]
fn a_guess_under_common_files_is_dropped_but_a_registered_location_is_not() {
    for root in [r"C:\Program Files", r"C:\Program Files (x86)"] {
        let mut e = entry("Premiere");
        e.display_icon = format!(r"{root}\Common Files\Adobe\Keyfiles\PremierePro\26.0\Icons\a.ico");
        e.uninstall_string = format!(r#""{root}\Common Files\Adobe\u.exe""#);
        assert_eq!(folder(&e), None, "{root}");
        e.install_location = format!(r"{root}\Common Files\Adobe\Own");
        assert_eq!(find(&e).map(|f| f.source), Some(FolderSource::InstallLocation));
    }
}

#[test]
fn temp_is_matched_by_whole_segments() {
    assert!(in_temp(r"c:\users\me\appdata\local\temp\x"));
    assert!(in_temp(r"d:\other\appdata\local\temp"));
    assert!(!in_temp(r"c:\users\me\appdata\local\temple run"));
    assert!(!in_temp(r"c:\users\me\appdata\local\programs\temp tool"));
    let mut e = entry("Temple Run");
    e.install_location = r"C:\Users\me\AppData\Local\Temple Run".into();
    // AppData\Local itself is a protected place, but a folder below it with a temp-like name is not.
    assert_eq!(folder(&e), Some(PathBuf::from(r"C:\Users\me\AppData\Local\Temple Run")));
}

#[test]
fn a_guess_that_is_or_holds_another_apps_folder_is_dropped() {
    // Premiere's icon sits in the Adobe folder that holds Photoshop's install location.
    let mut premiere = entry("Premiere");
    premiere.display_icon = r"C:\Program Files\Adobe\premiere.ico".into();
    premiere.uninstall_string = r#""C:\Program Files\Adobe\Uninstall\u.exe""#.into();
    let mut photoshop = entry("Photoshop");
    photoshop.install_location = r"C:\Program Files\Adobe\Adobe Photoshop 2026".into();
    let shown = build(vec![premiere.clone(), photoshop.clone()]);
    let premiere_row = shown.iter().find(|r| r.name == "Premiere").unwrap();
    // The Adobe folder is dropped; the uninstaller's own folder is not above anything.
    assert_eq!(premiere_row.folder.as_deref(), Some(r"C:\Program Files\Adobe\Uninstall"));
    assert!(premiere_row.folder_guessed);
    let photoshop_row = shown.iter().find(|r| r.name == "Photoshop").unwrap();
    assert_eq!(photoshop_row.folder.as_deref(), Some(r"C:\Program Files\Adobe\Adobe Photoshop 2026"));
    assert!(!photoshop_row.folder_guessed);

    // Two apps guessing the same folder: a guess never counts against another guess.
    let mut a = entry("A");
    a.key = "a".into();
    let mut b = entry("B");
    b.key = "b".into();
    let shown = build(vec![a, b]);
    assert!(shown.iter().all(|r| r.folder.as_deref() == Some(r"C:\Program Files\Acme")));

    // The rule holds for an InstallLocation too: a folder that holds another app's folder is a
    // vendor folder, not this app's. It falls through to a guess; the inner app keeps its own.
    let mut holder = entry("Holder");
    holder.install_location = r"D:\Games".into();
    let mut inner = entry("Inner");
    inner.install_location = r"D:\Games\Inner".into();
    let shown = build(vec![holder, inner]);
    let holder_row = shown.iter().find(|r| r.name == "Holder").unwrap();
    assert_eq!(holder_row.folder_source, Some(FolderSource::Uninstaller));
    let inner_row = shown.iter().find(|r| r.name == "Inner").unwrap();
    assert_eq!((inner_row.folder.as_deref(), inner_row.folder_guessed), (Some(r"D:\Games\Inner"), false));
}

#[test]
fn another_apps_guess_never_removes_a_registered_folder() {
    let mut foo = entry("Foo");
    foo.install_location = r"C:\Program Files\Foo".into();
    // B's only clue is an updater inside Foo's folder.
    let mut b = entry("Updater");
    b.uninstall_string = r#""C:\Program Files\Foo\updater\unins.exe""#.into();
    let shown = build(vec![foo, b]);
    let foo_row = shown.iter().find(|r| r.name == "Foo").unwrap();
    assert_eq!(foo_row.folder.as_deref(), Some(r"C:\Program Files\Foo"));
    assert!(!foo_row.folder_guessed);
    let b_row = shown.iter().find(|r| r.name == "Updater").unwrap();
    assert_eq!(b_row.folder.as_deref(), Some(r"C:\Program Files\Foo\updater"));
}

#[test]
fn two_apps_with_the_same_install_location_both_keep_it() {
    let mut product = entry("Product");
    product.install_location = r"D:\Apps\Product".into();
    let mut pack = entry("Product Language Pack");
    pack.install_location = r"D:\Apps\Product\".into();
    let mut twin = product.clone();
    twin.hive = "HKCU".into();
    let shown = build(vec![product, pack, twin]);
    assert_eq!(shown.len(), 3);
    assert!(shown.iter().all(|r| r.folder.as_deref() == Some(r"D:\Apps\Product") && !r.folder_guessed));
    // A guess equal to someone's registered folder is rejected.
    let mut guesser = entry("Guesser");
    guesser.uninstall_string = r#""D:\Apps\Product\unins.exe""#.into();
    let mut owner = entry("Owner");
    owner.install_location = r"D:\Apps\Product".into();
    let shown = build(vec![guesser, owner]);
    assert_eq!(shown.iter().find(|r| r.name == "Guesser").unwrap().folder, None);
}

#[test]
fn a_suite_above_its_plugins_registered_folder_loses_it() {
    // Accepted behaviour, pinned: a registered folder strictly above another app's registered
    // folder is a vendor folder. The suite falls through to its other clues.
    let mut suite = entry("Suite");
    suite.install_location = r"D:\Suite".into();
    let mut plugin = entry("Suite Plugin");
    plugin.install_location = r"D:\Suite\Plugins\One".into();
    let shown = build(vec![suite, plugin]);
    let suite_row = shown.iter().find(|r| r.name == "Suite").unwrap();
    assert_eq!(suite_row.folder_source, Some(FolderSource::Uninstaller));
    assert_eq!(shown.iter().find(|r| r.name == "Suite Plugin").unwrap().folder.as_deref(), Some(r"D:\Suite\Plugins\One"));
}

#[test]
fn found_by_id_gives_the_list_row_entry_and_command_with_the_others_known() {
    let mut premiere = entry("Premiere");
    premiere.install_location = r"C:\Program Files\Adobe".into();
    premiere.uninstall_string = r#""C:\Program Files\Adobe\u.exe" /uninstall"#.into();
    premiere.display_icon = r"C:\Program Files\Adobe\p.ico".into();
    let mut photoshop = entry("Photoshop");
    photoshop.install_location = r"C:\Program Files\Adobe\Adobe Photoshop 2026".into();
    let entries = vec![premiere, photoshop];
    let all = all_there(|ctx| found_apps(entries.clone(), &places(), &[], ctx, &plus_two));
    let found = all.iter().find(|f| f.row.id == r"HKLM64\{Premiere}").unwrap();
    assert_eq!(found.row.folder, None, "the Adobe folder holds Photoshop's");
    assert_eq!(found.entry.key, "{Premiere}");
    assert_eq!(found.command, exe(r"C:\Program Files\Adobe\u.exe", "/uninstall"));
    // `found_by_id` is this on the real registry; `install_folder` alone would not know Photoshop.
    let alone = all_there(|ctx| find_folder(&entries[0], &places(), ctx, &[])).map(|f| f.path);
    assert_eq!(alone, Some(PathBuf::from(r"C:\Program Files\Adobe")));
    assert!(found_by_id("nonsense").is_none());
}

#[test]
fn the_row_says_where_its_folder_came_from() {
    let mut e = entry("App");
    e.install_location = r"D:\Games\App".into();
    let r = &build(vec![e.clone()])[0];
    assert_eq!((r.folder_source, r.folder_guessed), (Some(FolderSource::InstallLocation), false));
    e.install_location.clear();
    e.display_icon = r"C:\Program Files\App2\a.exe".into();
    let r = &build(vec![e.clone()])[0];
    assert_eq!((r.folder_source, r.folder_guessed), (Some(FolderSource::Icon), true));
    e.display_icon.clear();
    let r = &build(vec![e])[0];
    assert_eq!((r.folder_source, r.folder_guessed), (Some(FolderSource::Uninstaller), true));
    let mut none = entry("None");
    none.uninstall_string = format!("MsiExec.exe /X{GUID}");
    let r = &build(vec![none])[0];
    assert_eq!((r.folder_source, r.folder_guessed), (None, false));
}

#[test]
fn no_folder_is_still_listed() {
    let mut e = entry("Mystery");
    e.uninstall_string = format!("MsiExec.exe /X{GUID}");
    let shown = build(vec![e]);
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].folder, None);
    assert!(!shown[0].running);
}

// ---- the command --------------------------------------------------------------------------------------------

#[test]
fn parse_quoted_path_with_args() {
    assert_eq!(parse(r#""C:\Program Files\App\unins000.exe" /SILENT"#), exe(r"C:\Program Files\App\unins000.exe", "/SILENT"));
}

#[test]
fn parse_unquoted_path_with_spaces() {
    assert_eq!(
        parse(r"C:\Program Files\My App\uninstall.exe /S /D=C:\x"),
        exe(r"C:\Program Files\My App\uninstall.exe", r"/S /D=C:\x")
    );
    assert_eq!(parse(r"C:\Program Files\Booking.com Helper\un.exe"), exe(r"C:\Program Files\Booking.com Helper\un.exe", ""));
}

#[test]
fn an_unquoted_line_with_several_exe_candidates_takes_the_shortest_that_exists() {
    // `x.exe` is in the arguments, but nothing says which `.exe` is the program: ask the disk.
    let line = r"C:\a\u.exe uninstall x.exe";
    assert_eq!(parse(line), exe(r"C:\a\u.exe", "uninstall x.exe"));
    let only_long = |p: &str| p == r"C:\a\u.exe uninstall x.exe";
    assert_eq!(
        with_ctx(&|_| true, &only_long, |ctx| parse_uninstall_with(line, ctx)),
        exe(r"C:\a\u.exe uninstall x.exe", "")
    );
    let both = |p: &str| p.ends_with("u.exe") || p.ends_with("x.exe");
    assert_eq!(with_ctx(&|_| true, &both, |ctx| parse_uninstall_with(line, ctx)), exe(r"C:\a\u.exe", "uninstall x.exe"));
    // Nothing exists: the shortest.
    assert_eq!(with_ctx(&|_| true, &|_| false, |ctx| parse_uninstall_with(line, ctx)), exe(r"C:\a\u.exe", "uninstall x.exe"));
}

#[test]
fn only_an_absolute_exe_or_a_system_program_can_be_run() {
    let system = r"C:\Windows\System32";
    assert_eq!(
        parse(r"rundll32.exe setupapi.dll,InstallHinfSection DefaultUninstall 132 C:\x.inf"),
        exe(&format!(r"{system}\rundll32.exe"), r"setupapi.dll,InstallHinfSection DefaultUninstall 132 C:\x.inf")
    );
    assert_eq!(parse("RUNDLL32 shell32.dll,Foo"), exe(&format!(r"{system}\rundll32.exe"), "shell32.dll,Foo"));
    assert_eq!(parse("msiexec.exe /?"), exe(&format!(r"{system}\msiexec.exe"), "/?"));
    for bad in [
        r"C:\Windows\System32\cmd.exe /c del C:\x",
        r#""C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe" -File C:\x.ps1"#,
        r"powershell.exe -File C:\x.ps1",
        r"cmd /c del x",
        r"C:\Tools\PWSH.EXE -c x",
        r"C:\Windows\System32\reg.exe delete HKLM\x",
        r"C:\Windows\System32\sc.exe delete x",
        r#""C:\Windows\explorer.exe" C:\x"#,
        "uninstall.exe",
        r"..\u.exe",
        r"sub\u.exe",
        r#""setup.exe" /S"#,
        r"C:\a\uninstall.bat",
        r"C:\a\uninstall.msi",
        r"C:\a\noextension",
        "cmd.exe /c del x",
        "/S",
        "",
        "   ",
        r#""C:\Program Files\App\unins000.exe /S"#,
        r#""C:\a b\u.exe"#,
    ] {
        assert_eq!(parse(bad), None, "{bad:?}");
    }
}

#[test]
fn shells_script_hosts_and_system_tools_are_never_run() {
    for name in NEVER_RUN {
        let upper = name.to_uppercase();
        for line in [format!(r"C:\Windows\System32\{name}.exe /x"), format!(r#""C:\Any Where\{upper}.EXE" /x"#), format!("{name} /x")] {
            assert_eq!(parse(&line), None, "{line}");
        }
    }
    // Ordinary uninstallers with similar names are fine.
    assert!(parse(r"C:\a\recovery.exe /x").is_some());
    assert!(parse(r"C:\a\scan.exe").is_some());
}

#[test]
fn parse_msiexec_x_and_i_forms_become_uninstall() {
    let want = Some(UninstallCommand::Msi { guid: GUID.into() });
    assert_eq!(parse(&format!("MsiExec.exe /I{GUID}")), want);
    assert_eq!(parse(&format!("MsiExec.exe /X{GUID}")), want);
    assert_eq!(parse(&format!("msiexec /x {GUID} /qn")), want);
    assert_eq!(parse(&format!(r"C:\Windows\System32\msiexec.exe /X{GUID}")), want);
    assert_eq!(parse(&format!(r#""C:\Windows\System32\msiexec.exe" /X{GUID}"#)), want);
    // Not a product code: left as a plain command, never as an Msi.
    for not_guid in ["{AAAA-BBBB}", "{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE", "{GAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}", "{AAAAAAAABBBBCCCCDDDDEEEEEEEEEEEE}"] {
        match parse(&format!("MsiExec.exe /X{not_guid}")) {
            Some(UninstallCommand::Exe { args, .. }) => assert!(args.contains(not_guid)),
            other => panic!("{not_guid}: {other:?}"),
        }
    }
}

#[test]
fn parse_expands_environment_variables_it_is_given() {
    assert_eq!(parse(r"%SEEK_APPS_TEST_DIR%\u.exe -y"), exe(r"C:\Tools\u.exe", "-y"));
    // An unknown variable stays as written and is not an absolute path.
    assert_eq!(parse(r"%NOPE%\u.exe"), None);
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
    for bad in [
        "",
        "HKLM64",
        r"HKLM64\",
        r"HKLM64\a\b",
        r"HKLM99\x",
        r"HKXX64\x",
        r"hklm64\x",
        r"HKLM6\x",
        "ünï\\x",
        r"HKLM64\..\x",
        "HKLM64\\a\u{0}b",
        "HKLM64\\a\nb",
        "HKLM64\\a\tb",
        "HKLM64\\\u{7f}",
        "HKLM64\\\u{0}",
    ] {
        assert_eq!(parse_app_id(bad), None, "{bad:?}");
    }
}

#[test]
fn entry_by_id_goes_through_the_reader_and_the_list_rules() {
    let mut seek = entry("Seek");
    seek.publisher = "Seek".into();
    seek.key = "Seek".into();
    let mut sys = entry("Sys");
    sys.system_component = true;
    let mut child = entry("Child");
    child.parent_key_name = "P".into();
    let mut quiet = entry("Quiet");
    quiet.uninstall_string.clear();
    quiet.quiet_uninstall_string = "x.exe /S".into();
    let fine64 = entry("Fine");
    let mut fine32 = fine64.clone();
    fine32.view = "32".into();
    let mut only32 = entry("Only32");
    only32.view = "32".into();
    let all = [seek, sys, child, quiet, fine64, fine32, only32];
    let read = |h: &str, v: &str, k: &str| all.iter().find(|e| e.hive == h && e.view == v && e.key == k).cloned();
    for hidden in [r"HKLM64\Seek", r"HKLM64\{Sys}", r"HKLM64\{Child}", r"HKLM64\{Quiet}", r"HKLM32\{Fine}", r"HKLM64\{Missing}", "HKLM64\\a\u{0}"] {
        assert!(entry_by_id_with(hidden, &read).is_none(), "{hidden}");
    }
    assert_eq!(entry_by_id_with(r"HKLM64\{Fine}", &read).map(|e| e.view), Some("64".to_string()));
    // Listed only from the 32-bit view: reachable there, as the list shows it.
    assert_eq!(entry_by_id_with(r"HKLM32\{Only32}", &read).map(|e| e.key), Some("{Only32}".to_string()));
}

#[test]
fn entry_by_id_refuses_a_malformed_id_and_a_missing_key() {
    assert!(entry_by_id("nonsense").is_none());
    assert!(entry_by_id("HKCU64\\a\u{0}b").is_none());
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
    let shown = all_there(|ctx| rows(vec![e.clone()], &places(), &exes, ctx, &plus_two));
    assert!(shown[0].running);
}

#[test]
fn running_uses_only_a_folder_that_survived_the_rules() {
    // The uninstaller sits in the Program Files root, which is rejected: no folder, so not running.
    let mut e = entry("Rooted");
    e.uninstall_string = r#""C:\Program Files\unins.exe""#.into();
    let exes = vec![r"C:\Program Files\other.exe".to_string()];
    let shown = all_there(|ctx| rows(vec![e], &places(), &exes, ctx, &plus_two));
    assert_eq!(shown[0].folder, None);
    assert!(!shown[0].running);
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
    all_there(|ctx| {
        assert_eq!(icon_source(&e, ctx), Some((r"C:\Program Files\Acme\unins000.exe".into(), 0)));
        e.display_icon = r"C:\x\app.exe,3".into();
        assert_eq!(icon_source(&e, ctx), Some((r"C:\x\app.exe".into(), 3)));
        e.display_icon.clear();
        e.uninstall_string = format!("MsiExec.exe /X{GUID}");
        assert_eq!(icon_source(&e, ctx), None);
    });
}

#[test]
fn only_fixed_local_drives_are_asked_for_icons_or_short_names() {
    let mut kinds = |l: char| match l {
        'C' | 'D' => 3,
        'E' => 2, // removable
        'F' => 5, // optical
        'Z' => 4, // network
        _ => 0,   // unknown
    };
    assert!(on_fixed_drive(r"C:\a\b.exe", &mut kinds));
    assert!(on_fixed_drive(r"d:\a\b.exe", &mut kinds));
    for p in [r"E:\a.exe", r"F:\a.exe", r"Z:\a.exe", r"Q:\a.exe", r"\\nas\share\a.exe", "a.exe", "", "C"] {
        assert!(!on_fixed_drive(p, &mut kinds), "{p:?}");
    }
    assert!(icons_win::extract_png(r"\\nohost\share\a.exe", 0).is_none());
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
fn clearing_the_cache_lets_go_of_every_icon() {
    use std::cell::Cell;
    let cache = IconCache::default();
    let calls = Cell::new(0);
    let extract = |_: &str, _: i32| {
        calls.set(calls.get() + 1);
        Some("i".to_string())
    };
    cache.get(r"C:\x\a.exe", 0, extract);
    cache.get(r"C:\x\a.exe", 0, extract);
    assert_eq!(calls.get(), 1);
    cache.clear();
    cache.get(r"C:\x\a.exe", 0, extract);
    assert_eq!(calls.get(), 2);
}

#[test]
fn a_path_is_asked_of_the_disk_once() {
    use std::cell::Cell;
    let calls = Cell::new(0);
    let ask = memoised(|p: &str| {
        calls.set(calls.get() + 1);
        p.ends_with('1')
    });
    assert!(ask(r"C:\a1"));
    assert!(ask(r"c:\A1"));
    assert!(!ask(r"C:\b"));
    assert!(!ask(r"C:\b"));
    assert_eq!(calls.get(), 2);
}

#[test]
fn a_row_says_whether_its_uninstaller_can_be_started() {
    let mut odd = entry("Odd");
    odd.uninstall_string = "uninstall.exe".into();
    let mut msi = entry("Msi");
    msi.uninstall_string = format!("MsiExec.exe /X{GUID}");
    let shown = build(vec![odd, entry("Fine"), msi]);
    let flags: Vec<(&str, bool)> = shown.iter().map(|r| (r.name.as_str(), r.can_uninstall)).collect();
    assert_eq!(flags, [("Fine", true), ("Msi", true), ("Odd", false)]);
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
    clear_icon_cache();
    let t = std::time::Instant::now();
    let cold_rows = try_list().expect("the app list can be read");
    let cold = t.elapsed().as_millis();
    let t = std::time::Instant::now();
    let warm_rows = try_list().expect("the app list can be read");
    let warm = t.elapsed().as_millis();
    assert_eq!(cold_rows.len(), warm_rows.len());

    let no_folder = warm_rows.iter().filter(|r| r.folder.is_none()).count();
    let guessed: Vec<&AppRow> = warm_rows.iter().filter(|r| r.folder_guessed).collect();
    let no_icon = warm_rows.iter().filter(|r| r.icon.is_none()).count();
    let running = warm_rows.iter().filter(|r| r.running).count();
    let cant_start: Vec<&AppRow> = warm_rows.iter().filter(|r| !r.can_uninstall).collect();
    let seek = warm_rows.iter().any(|r| r.name.trim().eq_ignore_ascii_case("seek"));
    let mut out = String::new();
    let _ = writeln!(out, "apps listed: {}", warm_rows.len());
    let _ = writeln!(out, "cold (icons extracted): {cold} ms");
    let _ = writeln!(out, "warm (icons cached): {warm} ms");
    let _ = writeln!(out, "no folder: {no_folder}");
    let _ = writeln!(out, "guessed folder: {}", guessed.len());
    let _ = writeln!(out, "no icon: {no_icon}");
    let _ = writeln!(out, "running: {running}");
    let _ = writeln!(out, "can't start uninstaller: {}", cant_start.len());
    for r in &cant_start {
        let _ = writeln!(out, "  - {}", r.name);
    }
    let _ = writeln!(out, "Seek appears: {seek}");
    let _ = writeln!(out, "\nfirst 15 rows (name | publisher | version | size | folder | guessed | note):");
    for r in warm_rows.iter().take(15) {
        let _ = writeln!(
            out,
            "{} | {} | {} | {} | {} | {} | {}",
            r.name,
            r.publisher,
            r.version,
            r.size,
            r.folder.as_deref().unwrap_or("-"),
            r.folder_guessed,
            r.note.as_deref().unwrap_or("-")
        );
    }
    println!("{out}");
    let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.superpowers/sdd/2026-11-05-seek-decimator/task-12-real-list.txt");
    std::fs::write(file, out).unwrap();
}
