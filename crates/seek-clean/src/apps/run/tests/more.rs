//! Fix round 1: one at a time by the watch phase, the gate on listing what is left, the launch flags.

use super::*;

#[test]
fn a_start_is_refused_while_the_watch_says_running_even_though_the_process_ended() {
    // A temp-copy uninstaller: the process we started is gone at once, the entry is not.
    let w = world();
    let launcher = FakeLauncher::default();
    w.uninstalls.start(&launcher, Some(found(ID, exe(r"C:\x\a.exe", "")))).unwrap();
    w.uninstalls.join_waiter(ID);
    let other = r"HKLM64\{OTHER}";
    assert_eq!(w.uninstalls.start(&launcher, Some(found(other, exe(r"C:\x\b.exe", "")))), Err(ALREADY_RUNNING.to_string()));
    // The window was closed and opened again: starting the same app again is refused too.
    assert_eq!(w.uninstalls.start(&launcher, Some(found(ID, exe(r"C:\x\a.exe", "")))), Err(ALREADY_RUNNING.to_string()));
    assert_eq!(launcher.asked.lock().unwrap().len(), 1);
    // Unresolved after the wait: free again.
    w.advance(GIVE_UP_MS);
    assert_eq!(w.uninstalls.state(ID), Ok(UninstallPhase::Unresolved));
    w.uninstalls.start(&launcher, Some(found(other, exe(r"C:\x\b.exe", "")))).unwrap();
    // Finished: free again.
    w.uninstalls.join_waiter(other);
    w.present.store(false, Ordering::SeqCst);
    w.advance(RECHECK_MS);
    assert_eq!(w.uninstalls.state(other), Ok(UninstallPhase::Finished));
    w.uninstalls.start(&launcher, Some(found(ID, exe(r"C:\x\a.exe", "")))).unwrap();
}

#[test]
fn what_is_left_is_refused_while_windows_still_lists_the_app() {
    let w = world();
    let launcher = FakeLauncher::default();
    assert_eq!(w.uninstalls.leftovers_for(ID).unwrap_err(), NOT_STARTED);
    w.uninstalls.start(&launcher, Some(found(ID, exe(r"C:\x\a.exe", "")))).unwrap();
    w.uninstalls.join_waiter(ID);
    assert_eq!(w.uninstalls.leftovers_for(ID).unwrap_err(), STILL_LISTED);
    assert_eq!(STILL_LISTED, "Windows still lists this app as installed. Seek lists what's left once it's gone.");
    w.present.store(false, Ordering::SeqCst);
    assert_eq!(w.uninstalls.leftovers_for(ID).unwrap().id, ID);
}

#[test]
fn the_program_starts_in_its_own_folder_without_windows_error_boxes() {
    assert_eq!(launch_dir(r"C:\Program Files\Acme\unins000.exe").as_deref(), Some(r"C:\Program Files\Acme"));
    assert_eq!(launch_dir("unins000.exe"), None);
    use windows::Win32::UI::Shell::{SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS};
    for bit in [SEE_MASK_NOCLOSEPROCESS, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC] {
        assert_eq!(LAUNCH_MASK & bit, bit);
    }
}

#[test]
fn an_exe_path_must_be_absolute_and_end_in_exe() {
    for path in ["unins000.exe", r"..\unins000.exe", r"C:\x\unins000.bat", r"C:\x\unins000", r"\\server\share\x.txt"] {
        let cmd = UninstallCommand::Exe { path: path.into(), args: String::new() };
        assert!(launch_request(&cmd, r"C:\Windows").is_err(), "{path}");
    }
    let ok = UninstallCommand::Exe { path: r"C:\x\U.EXE".into(), args: String::new() };
    assert!(launch_request(&ok, r"C:\Windows").is_ok());
}
