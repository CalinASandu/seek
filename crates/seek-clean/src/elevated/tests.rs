use std::sync::Mutex;
use std::time::Duration;

use super::account::{check_account, current_user_sid, read_facts, AccountFacts};
use super::*;
use crate::engine::Engine;
use crate::reasons::ADMIN_SILENT;
use crate::paths::make_junction;

const SID: &str = "S-1-5-21-1-2-3-1001";

#[derive(Default)]
struct FakeRecycler {
    calls: Mutex<Vec<PathBuf>>,
    /// Err, and the item is gone, as a silent permanent delete would leave it.
    destroy: Mutex<HashSet<PathBuf>>,
}

impl Recycler for FakeRecycler {
    fn recycle(&self, paths: &[PathBuf]) -> Vec<Result<String, String>> {
        let mut out = Vec::new();
        for p in paths {
            self.calls.lock().unwrap().push(p.clone());
            if p.is_dir() {
                std::fs::remove_dir_all(p).unwrap();
            } else {
                std::fs::remove_file(p).unwrap();
            }
            out.push(if self.destroy.lock().unwrap().contains(p) { Err("gone for good".into()) } else { Ok("bin".into()) });
        }
        out
    }
}

#[derive(Default)]
struct FakeClaims(Mutex<HashSet<PathBuf>>);

impl ClaimCheck for FakeClaims {
    fn is_claimed(&self, folder: &Path) -> Option<String> {
        self.0.lock().unwrap().contains(folder).then(|| "SomeApp".to_string())
    }
}

struct Env {
    dir: tempfile::TempDir,
    places: Places,
    rec: Arc<FakeRecycler>,
    claims: Arc<FakeClaims>,
    live: Mutex<HashSet<DeviceId>>,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let b = dir.path();
        for d in ["win", "pf", "pd/Seek/indexes", "pd/Seek/journal", "users/me/docs", "users/me/appdata", "elsewhere"] {
            std::fs::create_dir_all(b.join(d)).unwrap();
        }
        let s = |p: &str| b.join(p).to_string_lossy().into_owned();
        let places = Places::for_tests(&s("win"), &[&s("pf")], &s("users"), &s("users/me"), &s("pd"), &[&s("users/me/appdata")]);
        Env { dir, places, rec: Arc::default(), claims: Arc::default(), live: Mutex::default() }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel.replace('/', "\\"))
    }

    fn file(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let p = self.p(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn folder(&self, rel: &str) -> PathBuf {
        let p = self.p(rel);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("data.bin"), b"some data").unwrap();
        p
    }

    /// A job item as the window would have sent it: what the disk says now.
    fn item(&self, id: u32, p: &Path, why: Why) -> JobItem {
        let st = RealFacts.stat(p).unwrap();
        JobItem { id, path: p.to_string_lossy().into_owned(), size: st.size, modified_ms: st.modified_ms, why }
    }

    fn live_device(&self) -> DeviceId {
        let id = DeviceId::new_v4();
        self.live.lock().unwrap().insert(id);
        id
    }

    fn check(&self, item: &JobItem) -> Result<(), String> {
        check_item(item, &self.places, self.claims.as_ref(), &self.live.lock().unwrap())
    }

    fn ctx(&self) -> ElevatedContext {
        ElevatedContext {
            places: self.places.clone(),
            facts: Arc::new(RealFacts),
            recycler: self.rec.clone(),
            claims: self.claims.clone(),
            guard: Arc::new(guard::ParentChainGuard::new(self.places.guarded_roots())),
            protected: Ok(vec![]),
            devices: Ok(self.live.lock().unwrap().clone()),
            account: Ok(()),
        }
    }
}

fn run(j: &ElevatedJob, c: &ElevatedContext) -> Vec<ItemOutcome> {
    process_job(j, c).items
}

fn job(items: Vec<JobItem>) -> ElevatedJob {
    ElevatedJob { user_sid: SID.into(), window_pid: 1, items }
}

fn reason(o: &ItemOutcome) -> &str {
    match &o.outcome {
        Outcome::Left { reason } => reason,
        Outcome::Moved { .. } => panic!("moved: {}", o.path),
    }
}

fn is_moved(o: &ItemOutcome) -> bool {
    matches!(o.outcome, Outcome::Moved { .. })
}

// ---- check_item: what only the elevated side refuses -------------------------------------------------

#[test]
fn a_path_outside_program_files_and_seek_data_is_refused() {
    let env = Env::new();
    let docs = env.folder("users/me/docs/Photos");
    let appdata = env.folder("users/me/appdata/App");
    let other = env.file("elsewhere/a.seekidx", b"x");
    for (p, why) in [(&docs, Why::Leftover), (&appdata, Why::Uninstall), (&other, Why::StaleData), (&docs, Why::Duplicate)] {
        assert!(env.check(&env.item(0, p, why)).is_err(), "{p:?} {why:?}");
    }
    // Program Files itself is a place, not an app's folder.
    assert!(env.check(&env.item(0, &env.p("pf"), Why::Leftover)).is_err());
    // A duplicate is never an admin job, even inside Program Files.
    let inside = env.folder("pf/App");
    assert!(env.check(&env.item(0, &inside, Why::Duplicate)).is_err());
    assert_eq!(env.check(&env.item(0, &inside, Why::Leftover)), Ok(()));
}

#[test]
fn a_lookalike_path_is_refused() {
    let env = Env::new();
    for rel in ["pf-extra/App", "pf2/App", "pfx/App"] {
        let p = env.folder(rel);
        assert!(env.check(&env.item(0, &p, Why::Leftover)).is_err(), "{rel}");
    }
    let idx = env.file("pd/Seek-old/indexes/a.seekidx", b"x");
    assert!(env.check(&env.item(0, &idx, Why::StaleData)).is_err());
}

#[test]
fn a_traversal_path_is_refused() {
    let env = Env::new();
    let target = env.folder("elsewhere/App");
    env.folder("pf/Real");
    // Reaches `elsewhere\App` by way of Program Files; and a path that starts out of it.
    let via_pf = format!("{}\\..\\elsewhere\\App", env.p("pf").display());
    let mut it = env.item(0, &target, Why::Leftover);
    it.path = via_pf;
    assert!(env.check(&it).is_err());
    let mut it = env.item(0, &env.p("pf\\Real"), Why::Leftover);
    it.path = format!("{}\\pf\\Real\\..\\Real", env.dir.path().display());
    assert!(env.check(&it).is_err(), "even one that lands back in Program Files");
}

#[test]
fn a_junction_in_program_files_to_somewhere_else_is_refused() {
    let env = Env::new();
    let outside = env.folder("elsewhere/Precious");
    let link = env.p("pf/Link");
    assert!(make_junction(&link, &outside), "needs a junction");
    let it = env.item(0, &link, Why::Leftover);
    assert!(env.check(&it).is_err());
    // A folder reached through a junction parent is refused too.
    let inner = link.join("data.bin");
    assert!(env.check(&env.item(0, &inner, Why::Uninstall)).is_err());
}

#[test]
fn a_claimed_folder_is_refused() {
    let env = Env::new();
    let p = env.folder("pf/Claimed");
    env.claims.0.lock().unwrap().insert(p.clone());
    let e = env.check(&env.item(0, &p, Why::Leftover)).unwrap_err();
    assert!(e.contains("using this folder"), "{e}");
}

#[test]
fn a_live_index_file_is_refused() {
    let env = Env::new();
    let id = env.live_device();
    let live = env.file(&format!("pd/Seek/indexes/{id}.seekidx"), b"x");
    assert!(env.check(&env.item(0, &live, Why::StaleData)).unwrap_err().contains("still has"));
    let live_log = env.file(&format!("pd/Seek/journal/{id}.log"), b"x");
    assert!(env.check(&env.item(0, &live_log, Why::StaleData)).is_err());
}

#[test]
fn a_live_device_id_is_recognised_in_any_spelling() {
    let env = Env::new();
    let id = env.live_device();
    let hyphen = id.hyphenated().to_string();
    for stem in [hyphen.to_uppercase(), id.simple().to_string(), format!("{{{hyphen}}}"), id.simple().to_string().to_uppercase()] {
        let p = env.file(&format!("pd/Seek/indexes/{stem}.seekidx"), b"x");
        assert!(env.check(&env.item(0, &p, Why::StaleData)).is_err(), "{stem}");
    }
}

#[test]
fn devices_json_is_refused() {
    let env = Env::new();
    let reg = env.file("pd/Seek/devices.json", b"{}");
    assert!(env.check(&env.item(0, &reg, Why::StaleData)).is_err());
    // And so is anything else that is not an index or a journal directly in its folder.
    for rel in ["pd/Seek/indexes/devices.json", "pd/Seek/journal/a.seekidx", "pd/Seek/indexes/sub/a.seekidx", "pd/Seek/devices.json.bad"] {
        let p = env.file(rel, b"x");
        assert!(env.check(&env.item(0, &p, Why::StaleData)).is_err(), "{rel}");
    }
}

#[test]
fn a_stale_file_is_accepted() {
    let env = Env::new();
    env.live_device();
    let gone = DeviceId::new_v4();
    let idx = env.file(&format!("pd/Seek/indexes/{gone}.seekidx"), b"x");
    let log = env.file(&format!("pd/Seek/journal/{gone}.log"), b"x");
    assert_eq!(env.check(&env.item(0, &idx, Why::StaleData)), Ok(()));
    assert_eq!(env.check(&env.item(1, &log, Why::StaleData)), Ok(()));
    // A name that is not a device id at all is not one of the service's files.
    let odd = env.file("pd/Seek/indexes/notes.seekidx", b"x");
    assert!(env.check(&env.item(2, &odd, Why::StaleData)).is_err());
}

// ---- devices.json -------------------------------------------------------------------------------------

#[test]
fn the_live_devices_come_from_the_service_file_and_a_bad_file_is_an_error() {
    let env = Env::new();
    let dir = env.p("pd/Seek");
    assert!(read_live_devices(&dir).is_err(), "missing");
    std::fs::write(dir.join("devices.json"), b"{ not json").unwrap();
    assert!(read_live_devices(&dir).is_err(), "malformed");
    std::fs::write(dir.join("devices.json"), b"[]").unwrap();
    assert!(read_live_devices(&dir).is_err(), "wrong shape");
    // Written by the service's own type, read back, offline devices included.
    let device = |state| seek_core::Device {
        id: DeviceId::new_v4(),
        identity: seek_core::DeviceIdentity::Volume { serial: 7, filesystem: "exFAT".into() },
        display_name: "Stick".into(),
        label: String::new(),
        filesystem: "exFAT".into(),
        kind: seek_core::DeviceKind::Removable,
        total_bytes: 0,
        free_bytes: 0,
        last_mount_point: "E:\\".into(),
        indexed_by_host: DeviceId::new_v4(),
        first_seen: 0,
        last_seen: 0,
        index_updated_at: 0,
        state,
        entry_count: 0,
        error: None,
    };
    let (a, b) = (device(seek_core::DeviceState::Online), device(seek_core::DeviceState::Offline));
    let reg = DeviceRegistry { host_id: DeviceId::new_v4(), devices: vec![a.clone(), b.clone()] };
    reg.save(&dir).unwrap();
    assert_eq!(read_live_devices(&dir), Ok(HashSet::from([a.id, b.id])));
}

// ---- the whole run ------------------------------------------------------------------------------------

#[test]
fn an_item_that_passes_everything_is_recycled_and_its_neighbours_are_not_touched() {
    let env = Env::new();
    let app = env.folder("pf/OldApp");
    let other = env.folder("pf/Keep");
    let gone = DeviceId::new_v4();
    let stale = env.file(&format!("pd/Seek/indexes/{gone}.seekidx"), b"index");
    let out = run(&job(vec![env.item(0, &app, Why::Leftover), env.item(1, &stale, Why::StaleData)]), &env.ctx());
    assert!(out.iter().all(is_moved), "{out:?}");
    assert!(!app.exists() && !stale.exists() && other.exists());
    assert_eq!(env.rec.calls.lock().unwrap().len(), 2);
}

#[test]
fn an_item_in_the_wrong_place_is_not_handed_to_the_recycler() {
    let env = Env::new();
    let docs = env.folder("users/me/docs/Mine");
    let out = run(&job(vec![env.item(0, &docs, Why::Leftover)]), &env.ctx());
    assert!(!is_moved(&out[0]));
    assert!(docs.exists() && env.rec.calls.lock().unwrap().is_empty());
}

#[test]
fn a_protected_folder_is_refused() {
    let env = Env::new();
    let app = env.folder("pf/Precious");
    let inner_protected = env.folder("pf/Holder");
    let mut ctx = env.ctx();
    ctx.protected = Ok(vec![app.to_string_lossy().into_owned(), inner_protected.join("data.bin").to_string_lossy().into_owned()]);
    let out = run(&job(vec![env.item(0, &app, Why::Leftover), env.item(1, &inner_protected, Why::Leftover)]), &ctx);
    assert!(reason(&out[0]).contains("protected"), "{}", reason(&out[0]));
    assert!(reason(&out[1]).contains("protected"), "{}", reason(&out[1]));
    assert!(app.exists() && inner_protected.exists() && env.rec.calls.lock().unwrap().is_empty());
}

#[test]
fn unreadable_settings_refuse_everything() {
    let env = Env::new();
    let app = env.folder("pf/App");
    let mut ctx = env.ctx();
    ctx.protected = Err("Your settings file couldn't be read.".into());
    let out = run(&job(vec![env.item(0, &app, Why::Leftover)]), &ctx);
    assert!(reason(&out[0]).contains("settings file"));
    assert!(app.exists() && env.rec.calls.lock().unwrap().is_empty());
}

/// Says "not claimed" to the first two questions (what `check_item` asks about one folder) and
/// "claimed" to every question after: a program that starts while the engine is looking.
#[derive(Default)]
struct LateClaims(Mutex<u32>);

impl ClaimCheck for LateClaims {
    fn is_claimed(&self, _folder: &Path) -> Option<String> {
        let mut n = self.0.lock().unwrap();
        *n += 1;
        (*n > 2).then(|| "LateApp".to_string())
    }
}

#[test]
fn the_engines_own_rules_apply_here_too() {
    let env = Env::new();
    // Two levels deep: the engine's scan-root rule for leftovers.
    let deep = env.folder("pf/App/Inner");
    let mut ctx = env.ctx();
    ctx.claims = Arc::new(LateClaims::default());
    let late = env.folder("pf/Late");
    // check_item accepts this folder; only the engine's own Claimed rule refuses it.
    assert_eq!(check_item(&env.item(0, &late, Why::Leftover), &env.places, &LateClaims::default(), &HashSet::new()), Ok(()));
    let out = run(&job(vec![env.item(0, &late, Why::Leftover)]), &ctx);
    assert!(reason(&out[0]).contains("using this folder"), "{}", reason(&out[0]));
    assert!(late.exists() && env.rec.calls.lock().unwrap().is_empty());
    let out = run(&job(vec![env.item(0, &deep, Why::Leftover)]), &env.ctx());
    assert!(!is_moved(&out[0]) && deep.exists(), "{out:?}");
    // Shared with Windows and other apps: an uninstall never offers anything in it.
    let shared = env.folder("pf/Common Files/Thing");
    let out = run(&job(vec![env.item(0, &shared, Why::Uninstall)]), &env.ctx());
    assert!(!is_moved(&out[0]) && shared.exists(), "{out:?}");
}

#[test]
fn an_item_that_changed_since_review_is_left() {
    let env = Env::new();
    let grown = env.folder("pf/Grown");
    let touched = env.folder("pf/Touched");
    let gone = env.folder("pf/Vanished");
    let items = vec![env.item(0, &grown, Why::Leftover), env.item(1, &touched, Why::Leftover), env.item(2, &gone, Why::Leftover)];
    std::fs::write(grown.join("more.bin"), b"added while the prompt was open").unwrap();
    let mtime = std::fs::metadata(touched.join("data.bin")).unwrap().modified().unwrap() + Duration::from_secs(5);
    std::fs::File::options().write(true).open(touched.join("data.bin")).unwrap().set_modified(mtime).unwrap();
    std::fs::remove_dir_all(&gone).unwrap();
    let out = run(&job(items), &env.ctx());
    assert_eq!(reason(&out[0]), CHANGED);
    assert_eq!(reason(&out[1]), CHANGED);
    assert_eq!(reason(&out[2]), GONE);
    assert!(grown.exists() && touched.exists() && env.rec.calls.lock().unwrap().is_empty());
}

#[test]
fn a_different_account_is_refused() {
    let env = Env::new();
    let app = env.folder("pf/App");
    let own = AccountFacts {
        own_sid: Ok(SID.into()),
        own_image: Ok(PathBuf::from(r"C:\Seek\seek-app.exe")),
        window_sid: Ok(SID.into()),
        window_image: Ok(PathBuf::from(r"c:\seek\SEEK-APP.EXE")),
    };
    assert_eq!(check_account(SID, &own), Ok(()));
    let other_job = check_account("S-1-5-21-9-9-9-500", &own).unwrap_err();
    assert!(other_job.contains("account"), "{other_job}");
    let mut other_window = own.clone();
    other_window.window_sid = Ok("S-1-5-21-9-9-9-500".into());
    assert!(check_account(SID, &other_window).unwrap_err().contains("account"));
    let mut other_exe = own.clone();
    other_exe.window_image = Ok(PathBuf::from(r"C:\Temp\evil.exe"));
    assert!(check_account(SID, &other_exe).is_err());
    let mut unknown = own.clone();
    unknown.window_sid = Err("no such process".into());
    assert!(check_account(SID, &unknown).is_err());
    assert!(check_account("", &own).is_err(), "an empty SID matches nothing");
    // The elevated token's user differs from the job's: nothing at all is removed.
    let mut ctx = env.ctx();
    ctx.account = check_account("S-1-5-21-9-9-9-500", &own);
    let out = run(&job(vec![env.item(0, &app, Why::Leftover)]), &ctx);
    assert!(reason(&out[0]).contains("account"));
    assert!(app.exists() && env.rec.calls.lock().unwrap().is_empty());
}

#[test]
fn the_real_account_check_accepts_this_process_and_nothing_else() {
    let sid = current_user_sid().unwrap();
    assert!(sid.starts_with("S-1-"), "{sid}");
    let me = read_facts(std::process::id());
    assert_eq!(check_account(&sid, &me), Ok(()));
    assert!(check_account("S-1-5-21-9-9-9-500", &me).is_err());
    assert!(check_account(&sid, &read_facts(0)).is_err(), "a job with no window process");
}

#[test]
fn an_unreadable_device_list_refuses_stale_files_but_not_program_files() {
    let env = Env::new();
    let gone = DeviceId::new_v4();
    let stale = env.file(&format!("pd/Seek/indexes/{gone}.seekidx"), b"x");
    let app = env.folder("pf/App");
    let mut ctx = env.ctx();
    ctx.devices = Err("Seek's device list couldn't be read.".into());
    let out = run(&job(vec![env.item(0, &stale, Why::StaleData), env.item(1, &app, Why::Leftover)]), &ctx);
    assert!(reason(&out[0]).contains("device list"));
    assert!(is_moved(&out[1]));
    assert!(stale.exists());
}

#[test]
fn a_move_that_cannot_be_confirmed_stops_the_rest() {
    let env = Env::new();
    let first = env.folder("pf/A");
    let second = env.folder("pf/B");
    env.rec.destroy.lock().unwrap().insert(first.clone());
    let out = run(&job(vec![env.item(0, &first, Why::Leftover), env.item(1, &second, Why::Leftover)]), &env.ctx());
    assert!(reason(&out[0]).contains("deleted for good"), "{}", reason(&out[0]));
    assert!(reason(&out[1]).contains("stopped"), "{}", reason(&out[1]));
    assert!(second.exists());
    assert_eq!(env.rec.calls.lock().unwrap().len(), 1);
}

#[test]
fn a_repeated_item_number_or_path_is_not_removed_twice() {
    let env = Env::new();
    let app = env.folder("pf/App");
    let it = env.item(0, &app, Why::Leftover);
    let out = run(&job(vec![it.clone(), it.clone()]), &env.ctx());
    assert!(out.iter().all(|o| !is_moved(o)) && app.exists());
    let mut again = it.clone();
    again.id = 1;
    let out = run(&job(vec![it, again]), &env.ctx());
    assert!(is_moved(&out[0]) && !is_moved(&out[1]), "{out:?}");
}


// ---- I4: the all-users Start Menu ----------------------------------------------------------------------

#[test]
fn an_all_users_start_menu_shortcut_an_uninstall_left_is_accepted_and_nothing_else_in_program_data() {
    let env = Env::new();
    let menu = r"pd\Microsoft\Windows\Start Menu\Programs";
    let lnk = env.file(&format!(r"{menu}\Old App\Old App.lnk"), b"lnk");
    assert_eq!(env.check(&env.item(0, &lnk, Why::Uninstall)), Ok(()));
    // Not an uninstall's, not a shortcut, not in the all-users menu, not in `Programs` at all.
    assert!(env.check(&env.item(0, &lnk, Why::Leftover)).is_err());
    let txt = env.file(&format!(r"{menu}\Old App\readme.txt"), b"x");
    assert!(env.check(&env.item(0, &txt, Why::Uninstall)).is_err());
    let user = env.file(r"users\me\appdata\Roaming\Microsoft\Windows\Start Menu\Programs\Mine.lnk", b"lnk");
    assert!(env.check(&env.item(0, &user, Why::Uninstall)).is_err(), "the user's own menu is not admin's");
    let outside = env.file(r"pd\Microsoft\Windows\Start Menu\Other.lnk", b"lnk");
    assert!(env.check(&env.item(0, &outside, Why::Uninstall)).is_err());
    let other = env.file(r"pd\SomeVendor\thing.lnk", b"lnk");
    assert!(env.check(&env.item(0, &other, Why::Uninstall)).is_err());
    let folder = env.folder(&format!(r"{menu}\Folder"));
    assert!(env.check(&env.item(0, &folder, Why::Uninstall)).is_err(), "a folder there is not a shortcut");
    // The whole run takes it to the Recycle Bin.
    let out = run(&job(vec![env.item(0, &lnk, Why::Uninstall)]), &env.ctx());
    assert!(is_moved(&out[0]), "{out:?}");
    assert!(!lnk.exists());
}

// ---- M3 and M5 ------------------------------------------------------------------------------------------------

/// The real facts, except every stat after the first of a path says the size grew by one: what
/// changes while the engine is reading.
#[derive(Default)]
struct ChangingFacts(Mutex<HashMap<PathBuf, u32>>);

impl FileFacts for ChangingFacts {
    fn stat(&self, p: &Path) -> Option<crate::facts::Stat> {
        let mut s = RealFacts.stat(p)?;
        let mut seen = self.0.lock().unwrap();
        let n = seen.entry(p.to_path_buf()).or_insert(0);
        *n += 1;
        if *n > 1 {
            s.size += 1;
        }
        Some(s)
    }
    fn same_bytes(&self, a: &Path, b: &Path) -> Result<bool, String> {
        RealFacts.same_bytes(a, b)
    }
    fn file_id(&self, p: &Path) -> Option<(u32, u64)> {
        RealFacts.file_id(p)
    }
    fn recycle_block(&self, p: &Path, batch: u64) -> Option<crate::model::Lock> {
        RealFacts.recycle_block(p, batch)
    }
}

#[test]
fn an_item_the_engine_reads_differently_from_the_job_is_left_as_changed() {
    let env = Env::new();
    let app = env.folder("pf/App");
    let mut ctx = env.ctx();
    ctx.facts = Arc::new(ChangingFacts::default());
    let out = run(&job(vec![env.item(0, &app, Why::Leftover)]), &ctx);
    assert_eq!(reason(&out[0]), CHANGED);
    assert!(app.exists() && env.rec.calls.lock().unwrap().is_empty());
}

#[test]
fn a_stop_is_carried_back_in_the_result() {
    let env = Env::new();
    let first = env.folder("pf/A");
    let second = env.folder("pf/B");
    env.rec.destroy.lock().unwrap().insert(first.clone());
    let r = process_job(&job(vec![env.item(0, &first, Why::Leftover), env.item(1, &second, Why::Leftover)]), &env.ctx());
    assert!(r.stopped_early.as_deref().unwrap().contains("could not be confirmed"));
    let none = process_job(&job(vec![env.item(1, &second, Why::Leftover)]), &env.ctx());
    assert_eq!(none.stopped_early, None);
}

// ---- I1: the folders above an item are held -----------------------------------------------------------

fn elevated_engine(env: &Env) -> Engine {
    Engine::new_elevated(env.places.clone(), env.rec.clone(), Arc::new(RealFacts), env.claims.clone())
        .with_guard(Arc::new(guard::ParentChainGuard::new(env.places.guarded_roots())))
}

fn uninstall_candidate(p: &Path) -> Candidate {
    Candidate::Folder { path: p.to_path_buf(), why: Why::Uninstall, level: None, reasons: vec![] }
}

#[test]
fn a_parent_swapped_for_a_junction_after_prepare_leaves_the_item_in_place() {
    let env = Env::new();
    let item = env.folder("pf/Vendor/App");
    let decoy = env.folder("elsewhere/Precious");
    let engine = elevated_engine(&env);
    let prep = engine.prepare(vec![uninstall_candidate(&item)], &[]);
    assert_eq!(prep.items[0].lock, None, "{:?}", prep.items[0]);
    // After review: `pf\Vendor` becomes a junction to where the same-named folder tree now is.
    let moved = env.p("elsewhere/Vendor");
    std::fs::rename(env.p("pf/Vendor"), &moved).unwrap();
    assert!(make_junction(&env.p("pf/Vendor"), &moved), "needs a junction");
    let r = engine.commit(&prep.token, &[0], &[]).unwrap();
    assert_eq!(reason(&r.items[0]), guard::CHANGED_ABOVE);
    assert!(env.rec.calls.lock().unwrap().is_empty(), "nothing was handed to the recycler");
    assert!(moved.join("App").exists() && decoy.exists());
}

#[test]
fn without_a_swap_the_same_item_is_recycled_with_its_parents_held() {
    let env = Env::new();
    let item = env.folder("pf/Vendor/App");
    let engine = elevated_engine(&env);
    let prep = engine.prepare(vec![uninstall_candidate(&item)], &[]);
    let r = engine.commit(&prep.token, &[0], &[]).unwrap();
    assert!(is_moved(&r.items[0]), "{r:?}");
}

#[test]
fn holding_the_chain_stops_a_parent_being_renamed_until_it_is_released() {
    let env = Env::new();
    let item = env.folder("pf/Vendor/Sub/App");
    let g = guard::ParentChainGuard::new(env.places.guarded_roots());
    let held = g.hold(&item).unwrap();
    assert!(std::fs::rename(env.p("pf/Vendor"), env.p("pf/Renamed")).is_err(), "a held parent can't be renamed");
    assert!(std::fs::rename(env.p("pf/Vendor/Sub"), env.p("pf/Vendor/Sub2")).is_err(), "nor the one below it");
    // The item's own folder is not held: it can be recycled.
    drop(held);
    assert!(std::fs::rename(env.p("pf/Vendor"), env.p("pf/Renamed")).is_ok());
    // Items outside every root hold nothing.
    let stale = env.file("pd/Seek/indexes/a.seekidx", b"x");
    assert!(g.hold(&stale).is_ok());
    // A parent that is not where the path says (a link) is refused.
    let link_item = env.folder("elsewhere/Real/App");
    assert!(make_junction(&env.p("pf/Linked"), &env.p("elsewhere/Real")));
    let e = g.hold(&env.p("pf/Linked/App")).err().unwrap();
    assert_eq!(e, guard::CHANGED_ABOVE);
    assert!(link_item.exists());
}

// ---- I2: claims never trust the environment for system folders -----------------------------------------

#[test]
fn a_poisoned_program_files_variable_does_not_change_a_claim() {
    use crate::claims::{location_paths, trusted_env};
    let poisoned = |n: &str| match n {
        "ProgramFiles" | "programfiles" => Some(r"C:\Users\me\Evil".to_string()),
        "ProgramData" | "SystemRoot" | "windir" | "SystemDrive" | "ALLUSERSPROFILE" | "CommonProgramFiles(x86)" => Some(r"C:\Users\me\Evil".to_string()),
        "LOCALAPPDATA" => Some(r"C:\Users\me\AppData\Local".to_string()),
        _ => None,
    };
    let system = |n: &str| (n.eq_ignore_ascii_case("ProgramFiles")).then(|| r"C:\Program Files".to_string());
    let env = |n: &str| trusted_env(n, &poisoned, &system);
    assert_eq!(location_paths(r"%ProgramFiles%\Vendor App", &env), vec![r"C:\Program Files\Vendor App".to_string()]);
    assert_eq!(location_paths(r"%programfiles%\Vendor App", &env), vec![r"C:\Program Files\Vendor App".to_string()]);
    // A trusted name Windows can't answer is left unexpanded, never taken from the environment.
    assert!(location_paths(r"%ProgramData%\Vendor", &env).is_empty());
    assert!(location_paths(r"%windir%\x.exe", &env).is_empty());
    // Every other variable still comes from the environment.
    assert_eq!(location_paths(r"%LOCALAPPDATA%\Programs\X", &env), vec![r"C:\Users\me\AppData\Local\Programs\X".to_string()]);
    // The real system answers for each name, and for no other.
    for n in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)", "CommonProgramFiles", "CommonProgramW6432", "CommonProgramFiles(x86)", "ProgramData", "ALLUSERSPROFILE", "SystemRoot", "windir", "SystemDrive"] {
        let v = crate::protect::trusted_folder_env(n).unwrap_or_else(|| panic!("{n}"));
        assert!(v.len() >= 2 && v.as_bytes()[1] == b':', "{n}: {v}");
    }
    assert_eq!(crate::protect::trusted_folder_env("LOCALAPPDATA"), None);
}

// ---- C1 and M1: the pipe -------------------------------------------------------------------------------------

#[test]
fn a_pipe_name_of_the_wrong_shape_is_refused() {
    let good = pipe::new_name();
    assert!(pipe::name_ok(&good), "{good}");
    let hex = good.rsplit('-').next().unwrap().to_string();
    for bad in [
        String::new(),
        "job.json".to_string(),
        format!(r"\\.\pipe\seek-clean-{}", hex.to_uppercase()),
        format!(r"\\.\pipe\seek-clean-{}0", hex),
        format!(r"\\.\pipe\seek-clean-{}", &hex[..31]),
        format!(r"\\.\pipe\other-{hex}"),
        format!(r"\\server\pipe\seek-clean-{hex}"),
        format!(r"\\.\pipe\seek-clean-{hex}\..\x"),
        format!(r"C:\Temp\seek-clean-{hex}"),
        format!(r"\\.\pipe\seek-clean-{}g", &hex[..31]),
    ] {
        assert!(!pipe::name_ok(&bad), "{bad}");
        assert_eq!(run_main_with(&bad, &|_| panic!("no context for a bad name")), EXIT_REFUSED, "{bad}");
    }
}

/// Runs the elevated side of `name` on this thread, as the launcher of a test.
struct ClientLauncher(Box<dyn Fn(&str) -> u32 + Send + Sync>);

impl Launcher for ClientLauncher {
    fn launch_and_wait(&self, name: &str) -> Result<u32, LaunchError> {
        Ok((self.0)(name))
    }
}

fn client_of(env: &Arc<Env>) -> ClientLauncher {
    let env = env.clone();
    ClientLauncher(Box::new(move |name| run_main_with(name, &|_| Ok(env.ctx())) as u32))
}

#[test]
fn a_good_job_over_a_real_pipe_is_processed_and_answered() {
    let env = Arc::new(Env::new());
    let app = env.folder("pf/App");
    let docs = env.folder("users/me/docs/Mine");
    let items = vec![env.item(0, &app, Why::Leftover), env.item(1, &docs, Why::Leftover)];
    let out = run_elevated(&items, SID, std::process::id(), &client_of(&env));
    assert!(is_moved(&out.items[0]) && !is_moved(&out.items[1]), "{out:?}");
    assert!(!app.exists() && docs.exists());
    assert_eq!(out.stopped_early, None);
}

#[test]
fn a_server_pid_that_is_not_the_jobs_window_pid_refuses_everything() {
    let env = Arc::new(Env::new());
    let app = env.folder("pf/App");
    let items = vec![env.item(0, &app, Why::Leftover)];
    // The job says it comes from another process than the one serving the pipe.
    let other_pid = std::process::id().wrapping_add(4);
    let out = run_elevated(&items, SID, other_pid, &client_of(&env));
    assert!(reason(&out.items[0]).contains("not asked for by the Seek window"), "{}", reason(&out.items[0]));
    assert!(app.exists() && env.rec.calls.lock().unwrap().is_empty());
    // And with no window named at all.
    let out = run_elevated(&items, SID, 0, &client_of(&env));
    assert!(!is_moved(&out.items[0]) && app.exists());
}

/// Serves `job` as raw bytes to the elevated side and returns its exit code and what it answered.
fn serve_raw(env: &Arc<Env>, job: Vec<u8>) -> (i32, ElevatedResult) {
    let name = pipe::new_name();
    let server = pipe::Server::create(&name).unwrap();
    let serving = std::thread::spawn(move || server.serve(&job, MAX_RESULT_BYTES));
    let code = run_main_with(&name, &|_| Ok(env.ctx()));
    let reply = serving.join().unwrap().unwrap();
    (code, serde_json::from_slice(&reply).unwrap())
}

#[test]
fn a_malformed_job_over_a_real_pipe_refuses_everything() {
    let env = Arc::new(Env::new());
    let app = env.folder("pf/App");
    for bad in [&b"{ nope"[..], b"[]", b"", br#"{"userSid":"x","items":[{"id":"a"}]}"#] {
        let (code, result) = serve_raw(&env, bad.to_vec());
        assert_eq!(code, EXIT_REFUSED);
        assert!(result.items.is_empty() && result.note.unwrap().contains("malformed"));
    }
    // Too many items.
    let items: Vec<JobItem> = (0..=MAX_JOB_ITEMS as u32).map(|id| JobItem { id, path: "x".into(), size: 0, modified_ms: 0, why: Why::Leftover }).collect();
    let big = serde_json::to_vec(&job(items)).unwrap();
    assert!((big.len() as u64) < MAX_JOB_BYTES);
    let (code, result) = serve_raw(&env, big);
    assert_eq!(code, EXIT_REFUSED);
    assert!(result.note.unwrap().contains("too many"));
    // Too many bytes.
    let (code, result) = serve_raw(&env, vec![b' '; (MAX_JOB_BYTES + 1) as usize]);
    assert_eq!(code, EXIT_REFUSED);
    assert!(result.note.unwrap().contains("too big"));
    assert!(app.exists() && env.rec.calls.lock().unwrap().is_empty());
}

#[test]
fn a_context_that_cannot_be_built_refuses_every_item_with_its_reason() {
    let env = Arc::new(Env::new());
    let app = env.folder("pf/App");
    let items = vec![env.item(0, &app, Why::Leftover)];
    let job = serde_json::to_vec(&ElevatedJob { user_sid: SID.into(), window_pid: std::process::id(), items }).unwrap();
    let name = pipe::new_name();
    let server = pipe::Server::create(&name).unwrap();
    let serving = std::thread::spawn(move || server.serve(&job, MAX_RESULT_BYTES));
    let code = run_main_with(&name, &|_| Err("Windows can't name its folders.".into()));
    let reply: ElevatedResult = serde_json::from_slice(&serving.join().unwrap().unwrap()).unwrap();
    assert_eq!(code, EXIT_REFUSED);
    assert!(reason(&reply.items[0]).contains("can't name"));
    assert!(app.exists());
}

// ---- the window's side ----------------------------------------------------------------------------------

fn sent(n: u32) -> Vec<JobItem> {
    (0..n).map(|id| JobItem { id, path: format!(r"C:\Program Files\App{id}"), size: 10 + id as u64, modified_ms: 5, why: Why::Leftover }).collect()
}

fn answer(id: u32, outcome: Outcome) -> ItemOutcome {
    ItemOutcome { id, path: "ignored".into(), size: 0, outcome }
}

fn result_bytes(items: Vec<ItemOutcome>, note: Option<&str>) -> Vec<u8> {
    serde_json::to_vec(&ElevatedResult { items, note: note.map(str::to_string), stopped_early: None }).unwrap()
}

#[test]
fn results_are_matched_to_the_ids_that_were_sent() {
    let sent = sent(3);
    let moved = || Outcome::Moved { bin_path: "bin".into() };
    let bytes = result_bytes(vec![answer(2, moved()), answer(99, moved()), answer(0, Outcome::Left { reason: "in use".into() }), answer(0, moved())], None);
    let out = match_results(&sent, Some(&bytes), 0).items;
    assert_eq!(out.len(), 3, "one outcome per item sent, none for id 99");
    assert_eq!(reason(&out[0]), "in use", "the first answer for an id wins");
    assert_eq!(reason(&out[1]), ADMIN_SILENT);
    assert!(is_moved(&out[2]));
    assert_eq!((out[2].path.as_str(), out[2].size), (r"C:\Program Files\App2", 12), "path and size are what was sent");
}

#[test]
fn an_item_with_no_answer_may_have_moved_unless_the_run_refused() {
    let sent = sent(2);
    for bytes in [None, Some(&b""[..]), Some(&b"{ half"[..]), Some(&b"[1,2]"[..]), Some(&br#"{"items":[{"id":0}]}"#[..])] {
        for code in [0u32, 1, 3, 259, u32::MAX] {
            let out = match_results(&sent, bytes, code).items;
            assert!(out.iter().all(|o| reason(o) == ADMIN_SILENT), "{bytes:?} {code}");
            assert!(ADMIN_SILENT.contains("Check the Recycle Bin") && !ADMIN_SILENT.contains("left in place"));
        }
        // Exit 2: nothing was done.
        let out = match_results(&sent, bytes, 2).items;
        assert!(out.iter().all(|o| reason(o) == REFUSED_RUN), "{bytes:?}");
    }
    let partial = result_bytes(vec![answer(1, Outcome::Moved { bin_path: "b".into() })], None);
    let out = match_results(&sent, Some(&partial), 1).items;
    assert_eq!(reason(&out[0]), ADMIN_SILENT);
    assert!(is_moved(&out[1]));
    // Exit 2 with the elevated side's own reason passes it on.
    let noted = result_bytes(vec![], Some("The job is malformed, so nothing was removed."));
    assert!(reason(&match_results(&sent, Some(&noted), 2).items[0]).contains("malformed"));
    // Even with exit 0 a note does not turn a silent item into "left".
    assert_eq!(reason(&match_results(&sent, Some(&noted), 0).items[0]), ADMIN_SILENT);
}

#[test]
fn a_stop_reported_by_the_elevated_side_reaches_the_answer() {
    let sent = sent(1);
    let bytes = serde_json::to_vec(&ElevatedResult { items: vec![], note: None, stopped_early: Some("C:\\x could not be confirmed".into()) }).unwrap();
    assert_eq!(match_results(&sent, Some(&bytes), 0).stopped_early.as_deref(), Some("C:\\x could not be confirmed"));
    assert_eq!(match_results(&sent, None, 0).stopped_early, None);
}

struct FailingLauncher(LaunchError);

impl Launcher for FailingLauncher {
    fn launch_and_wait(&self, _name: &str) -> Result<u32, LaunchError> {
        Err(self.0.clone())
    }
}

#[test]
fn a_declined_prompt_says_so_for_each_item_and_a_failed_start_gives_the_error_text() {
    let out = run_elevated(&sent(2), SID, 1, &FailingLauncher(LaunchError::Declined));
    assert!(out.items.iter().all(|o| reason(o) == "Windows asked for admin and it was declined."));
    let out = run_elevated(&sent(1), SID, 1, &FailingLauncher(LaunchError::Failed("Access is denied. (os error 5)".into())));
    assert!(reason(&out.items[0]).contains("Access is denied"));
}

#[test]
fn a_launch_that_never_connects_does_not_hang_and_leaves_the_items_to_the_exit_code() {
    // The elevated copy started and exited without ever opening the pipe (exit code 1).
    let l = ClientLauncher(Box::new(|_| 1));
    let out = run_elevated(&sent(2), SID, std::process::id(), &l);
    assert!(out.items.iter().all(|o| reason(o) == ADMIN_SILENT));
    let l = ClientLauncher(Box::new(|_| 2));
    let out = run_elevated(&sent(1), SID, std::process::id(), &l);
    assert_eq!(reason(&out.items[0]), REFUSED_RUN);
}

#[test]
fn every_run_uses_its_own_pipe_and_makes_one_launch() {
    let names = Arc::new(Mutex::new(Vec::new()));
    let seen = names.clone();
    let l = ClientLauncher(Box::new(move |n| {
        seen.lock().unwrap().push(n.to_string());
        1
    }));
    run_elevated(&sent(1), SID, 1, &l);
    run_elevated(&sent(1), SID, 1, &l);
    let names = names.lock().unwrap();
    assert_eq!(names.len(), 2);
    assert_ne!(names[0], names[1]);
    assert!(names.iter().all(|n| pipe::name_ok(n)));
}

#[test]
fn the_job_elevator_makes_one_launch_for_a_whole_batch() {
    let calls = Arc::new(Mutex::new(0u32));
    let c = calls.clone();
    let e = JobElevator { launcher: Arc::new(ClientLauncher(Box::new(move |_| {
        *c.lock().unwrap() += 1;
        1
    }))) };
    let out = e.run(&sent(3)).unwrap();
    assert_eq!(*calls.lock().unwrap(), 1);
    assert_eq!(out.items.len(), 3);
}

#[test]
fn the_pipe_cannot_be_made_twice() {
    let name = pipe::new_name();
    let _first = pipe::Server::create(&name).unwrap();
    assert!(pipe::Server::create(&name).is_err(), "FILE_FLAG_FIRST_PIPE_INSTANCE: no squatting before the window");
}

