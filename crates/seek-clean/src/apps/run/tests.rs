//! Nothing here starts a program: the launcher is a fake that records what it was asked to run.

use std::sync::atomic::AtomicU64;
use std::sync::mpsc;

use super::*;
use crate::apps::{parse_uninstall_with, Ctx};
use crate::model::FolderSource;

const GUID: &str = "{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}";

fn row(id: &str) -> AppRow {
    AppRow {
        id: id.into(),
        name: "Acme Editor".into(),
        publisher: "Acme".into(),
        version: "1.0".into(),
        size: 0,
        install_date: None,
        icon: None,
        folder: Some(r"C:\Program Files\Acme Editor".into()),
        folder_source: Some(FolderSource::InstallLocation),
        folder_guessed: false,
        can_uninstall: true,
        note: None,
        running: false,
    }
}

fn found(id: &str, command: Option<UninstallCommand>) -> FoundApp {
    FoundApp { row: row(id), entry: Default::default(), command }
}

fn exe(path: &str, args: &str) -> Option<UninstallCommand> {
    Some(UninstallCommand::Exe { path: path.into(), args: args.into() })
}

/// A launcher that starts nothing. Each launch hands out a waiter that ends at once, or, with
/// `hold`, one that ends when the test says so.
#[derive(Default)]
struct FakeLauncher {
    asked: Mutex<Vec<UninstallCommand>>,
    hold: Mutex<Option<mpsc::Receiver<()>>>,
    fail: Mutex<Option<String>>,
}

struct FakeWait(Option<mpsc::Receiver<()>>);

impl ProcessWait for FakeWait {
    fn wait(self: Box<Self>) {
        if let Some(rx) = self.0 {
            let _ = rx.recv();
        }
    }
}

impl Launcher for FakeLauncher {
    fn launch(&self, cmd: &UninstallCommand) -> Result<Box<dyn ProcessWait>, String> {
        self.asked.lock().unwrap().push(cmd.clone());
        if let Some(e) = self.fail.lock().unwrap().clone() {
            return Err(e);
        }
        Ok(Box::new(FakeWait(self.hold.lock().unwrap().take())))
    }
}

impl FakeLauncher {
    /// The next launch ends only when the returned sender sends (or is dropped).
    fn hold_next(&self) -> mpsc::Sender<()> {
        let (tx, rx) = mpsc::channel();
        *self.hold.lock().unwrap() = Some(rx);
        tx
    }
}

/// A clock the test moves, and an entry reader it controls (counting the reads).
struct World {
    now: Arc<AtomicU64>,
    present: Arc<AtomicBool>,
    reads: Arc<AtomicU64>,
    uninstalls: Uninstalls,
}

fn world() -> World {
    let now = Arc::new(AtomicU64::new(0));
    let present = Arc::new(AtomicBool::new(true));
    let reads = Arc::new(AtomicU64::new(0));
    let (n, p, r) = (now.clone(), present.clone(), reads.clone());
    let uninstalls = Uninstalls::with(
        Arc::new(move || n.load(Ordering::SeqCst)),
        Arc::new(move |_| {
            r.fetch_add(1, Ordering::SeqCst);
            p.load(Ordering::SeqCst)
        }),
    );
    World { now, present, reads, uninstalls }
}

impl World {
    fn advance(&self, ms: u64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }

    fn reads(&self) -> u64 {
        self.reads.load(Ordering::SeqCst)
    }
}

const ID: &str = r"HKLM64\{ACME-1}";

#[test]
fn finished_needs_the_process_ended_and_the_entry_gone() {
    assert_eq!(is_finished(true, false), UninstallPhase::Finished);
    assert_eq!(is_finished(true, true), UninstallPhase::Running);
    assert_eq!(is_finished(false, false), UninstallPhase::Running);
    assert_eq!(is_finished(false, true), UninstallPhase::Running);
}

#[test]
fn process_ended_with_entry_still_there_is_unresolved_after_the_wait() {
    let w = world();
    let launcher = FakeLauncher::default();
    w.uninstalls.start(&launcher, Some(found(ID, exe(r"C:\x\unins000.exe", "")))).unwrap();
    w.uninstalls.join_waiter(ID);
    assert_eq!(w.uninstalls.state(ID), Ok(UninstallPhase::Running));
    w.advance(GIVE_UP_MS - 1);
    assert_eq!(w.uninstalls.state(ID), Ok(UninstallPhase::Running), "one millisecond short of the wait");
    w.advance(1);
    assert_eq!(w.uninstalls.state(ID), Ok(UninstallPhase::Unresolved));
}

#[test]
fn an_entry_that_goes_during_the_wait_ends_it_as_finished() {
    let w = world();
    let launcher = FakeLauncher::default();
    w.uninstalls.start(&launcher, Some(found(ID, exe(r"C:\x\unins000.exe", "")))).unwrap();
    w.uninstalls.join_waiter(ID);
    w.advance(10_000);
    assert_eq!(w.uninstalls.state(ID), Ok(UninstallPhase::Running));
    w.present.store(false, Ordering::SeqCst);
    w.advance(RECHECK_MS);
    assert_eq!(w.uninstalls.state(ID), Ok(UninstallPhase::Finished));
    // Finished stays finished, with no more reading.
    let reads = w.reads();
    w.present.store(true, Ordering::SeqCst);
    w.advance(60_000);
    assert_eq!(w.uninstalls.state(ID), Ok(UninstallPhase::Finished));
    assert_eq!(w.reads(), reads);
}

#[test]
fn the_entry_is_read_once_a_second_and_not_at_all_while_the_process_runs() {
    let w = world();
    let launcher = FakeLauncher::default();
    let release = launcher.hold_next();
    w.uninstalls.start(&launcher, Some(found(ID, exe(r"C:\x\unins000.exe", "")))).unwrap();
    for _ in 0..5 {
        w.advance(700);
        assert_eq!(w.uninstalls.state(ID), Ok(UninstallPhase::Running));
    }
    assert_eq!(w.reads(), 0, "the registry is not read before the process ends");
    drop(release);
    w.uninstalls.join_waiter(ID);
    // Ten questions inside one second read it once.
    for _ in 0..10 {
        assert_eq!(w.uninstalls.state(ID), Ok(UninstallPhase::Running));
    }
    assert_eq!(w.reads(), 1);
    w.advance(RECHECK_MS - 1);
    w.uninstalls.state(ID).unwrap();
    assert_eq!(w.reads(), 1);
    w.advance(1);
    w.uninstalls.state(ID).unwrap();
    assert_eq!(w.reads(), 2);
}

#[test]
fn a_running_process_holds_no_lock_the_app_needs() {
    let w = world();
    let launcher = FakeLauncher::default();
    let release = launcher.hold_next();
    w.uninstalls.start(&launcher, Some(found(ID, exe(r"C:\x\unins000.exe", "")))).unwrap();
    // The waiter is blocked right now, and the manager still answers.
    assert_eq!(w.uninstalls.state(ID), Ok(UninstallPhase::Running));
    assert_eq!(w.uninstalls.app(ID).unwrap().name, "Acme Editor");
    release.send(()).unwrap();
    w.uninstalls.join_waiter(ID);
    w.present.store(false, Ordering::SeqCst);
    assert_eq!(w.uninstalls.state(ID), Ok(UninstallPhase::Finished));
}

#[test]
fn the_state_is_kept_per_app() {
    let w = world();
    let launcher = FakeLauncher::default();
    w.uninstalls.start(&launcher, Some(found(ID, exe(r"C:\x\a.exe", "")))).unwrap();
    w.uninstalls.join_waiter(ID);
    let other = r"HKLM64\{OTHER}";
    assert_eq!(w.uninstalls.state(other), Err(NOT_STARTED.to_string()));
    w.uninstalls.start(&launcher, Some(found(other, exe(r"C:\x\b.exe", "")))).unwrap();
    w.uninstalls.join_waiter(other);
    w.present.store(false, Ordering::SeqCst);
    assert_eq!(w.uninstalls.state(other), Ok(UninstallPhase::Finished));
    assert_eq!(w.uninstalls.app(ID).unwrap().id, ID);
}

#[test]
fn start_refuses_an_app_the_list_does_not_have_or_one_without_a_command() {
    let w = world();
    let launcher = FakeLauncher::default();
    assert_eq!(w.uninstalls.start(&launcher, None), Err(NOT_LISTED.to_string()));
    assert_eq!(w.uninstalls.start(&launcher, Some(found(ID, None))), Err(NO_COMMAND.to_string()));
    assert!(launcher.asked.lock().unwrap().is_empty(), "nothing was started");
    assert_eq!(w.uninstalls.state(ID), Err(NOT_STARTED.to_string()));
}

#[test]
fn a_second_start_is_refused_while_one_is_running() {
    let w = world();
    let launcher = FakeLauncher::default();
    let release = launcher.hold_next();
    w.uninstalls.start(&launcher, Some(found(ID, exe(r"C:\x\a.exe", "")))).unwrap();
    let other = r"HKLM64\{OTHER}";
    assert_eq!(w.uninstalls.start(&launcher, Some(found(other, exe(r"C:\x\b.exe", "")))), Err(ALREADY_RUNNING.to_string()));
    assert_eq!(w.uninstalls.start(&launcher, Some(found(ID, exe(r"C:\x\a.exe", "")))), Err(ALREADY_RUNNING.to_string()));
    assert_eq!(launcher.asked.lock().unwrap().len(), 1);
    release.send(()).unwrap();
    w.uninstalls.join_waiter(ID);
    w.uninstalls.start(&launcher, Some(found(other, exe(r"C:\x\b.exe", "")))).unwrap();
    assert_eq!(launcher.asked.lock().unwrap().len(), 2);
}

#[test]
fn a_failed_launch_leaves_nothing_running_and_says_why() {
    let w = world();
    let launcher = FakeLauncher::default();
    *launcher.fail.lock().unwrap() = Some(NO_PERMISSION.to_string());
    assert_eq!(w.uninstalls.start(&launcher, Some(found(ID, exe(r"C:\x\a.exe", "")))), Err(NO_PERMISSION.to_string()));
    assert_eq!(w.uninstalls.state(ID), Err(NOT_STARTED.to_string()));
    *launcher.fail.lock().unwrap() = None;
    w.uninstalls.start(&launcher, Some(found(ID, exe(r"C:\x\a.exe", "")))).unwrap();
}

#[test]
fn the_launcher_gets_the_registered_command_and_only_that() {
    let w = world();
    let launcher = FakeLauncher::default();
    let cmd = UninstallCommand::Exe { path: r"C:\Program Files\Acme\unins000.exe".into(), args: "/SILENT /x".into() };
    w.uninstalls.start(&launcher, Some(found(ID, Some(cmd.clone())))).unwrap();
    assert_eq!(*launcher.asked.lock().unwrap(), vec![cmd]);
}

#[test]
fn an_exe_runs_with_exactly_the_registered_path_and_arguments() {
    let cmd = UninstallCommand::Exe { path: r"C:\Program Files\Acme\unins000.exe".into(), args: r#"/x "C:\a b""#.into() };
    let req = launch_request(&cmd, r"C:\Windows").unwrap();
    assert_eq!(req, LaunchRequest { program: r"C:\Program Files\Acme\unins000.exe".into(), args: r#"/x "C:\a b""#.into() });
}

#[test]
fn msi_always_runs_the_system_msiexec_with_only_the_product_code() {
    let cmd = UninstallCommand::Msi { guid: GUID.into() };
    let req = launch_request(&cmd, r"C:\Windows").unwrap();
    assert_eq!(req.program, r"C:\Windows\System32\msiexec.exe");
    assert_eq!(req.args, format!("/x{GUID}"));
    // A trailing separator on the Windows folder doesn't double up.
    assert_eq!(launch_request(&cmd, r"D:\WinNT\").unwrap().program, r"D:\WinNT\System32\msiexec.exe");
}

#[test]
fn msi_from_the_registry_loses_its_other_arguments() {
    // What the registry says is parsed, and only the product code survives into the request.
    let env = |n: &str| (n.eq_ignore_ascii_case("systemroot")).then(|| r"C:\Windows".to_string());
    let ctx = Ctx { env: &env, expand: &|p| p.to_string(), dir_exists: &|_| true, file_exists: &|_| true };
    let cmd = parse_uninstall_with(&format!(r"C:\Evil\msiexec.exe /X{GUID} /qn TRANSFORMS=evil.mst REBOOT=ReallySuppress"), &ctx).unwrap();
    assert_eq!(cmd, UninstallCommand::Msi { guid: GUID.into() });
    let req = launch_request(&cmd, r"C:\Windows").unwrap();
    assert_eq!(req, LaunchRequest { program: r"C:\Windows\System32\msiexec.exe".into(), args: format!("/x{GUID}") });
}

#[test]
fn a_malformed_product_code_or_an_empty_program_is_not_run() {
    for guid in ["{not-a-guid}", "", &format!("{GUID} & calc")] {
        assert!(launch_request(&UninstallCommand::Msi { guid: guid.into() }, r"C:\Windows").is_err(), "{guid}");
    }
    assert!(launch_request(&UninstallCommand::Msi { guid: GUID.into() }, "").is_err());
    assert!(launch_request(&UninstallCommand::Exe { path: "  ".into(), args: String::new() }, r"C:\Windows").is_err());
}

#[test]
fn a_phase_serializes_as_the_word_the_window_expects() {
    assert_eq!(serde_json::to_string(&UninstallPhase::Running).unwrap(), "\"running\"");
    assert_eq!(serde_json::to_string(&UninstallPhase::Finished).unwrap(), "\"finished\"");
    assert_eq!(serde_json::to_string(&UninstallPhase::Unresolved).unwrap(), "\"unresolved\"");
}

#[test]
fn a_malformed_app_id_has_no_registry_entry() {
    assert!(!entry_present("not an id"));
    assert!(!entry_present(r"HKLM64\a\b"));
}
