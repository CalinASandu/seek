use std::sync::Mutex;
use std::time::Duration;

use super::account::{check_account, current_user_sid, read_facts, AccountFacts};
use super::*;
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
            protected: Ok(vec![]),
            devices: Ok(self.live.lock().unwrap().clone()),
            account: Ok(()),
        }
    }
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
    let out = process_job(&job(vec![env.item(0, &app, Why::Leftover), env.item(1, &stale, Why::StaleData)]), &env.ctx());
    assert!(out.iter().all(is_moved), "{out:?}");
    assert!(!app.exists() && !stale.exists() && other.exists());
    assert_eq!(env.rec.calls.lock().unwrap().len(), 2);
}

#[test]
fn an_item_in_the_wrong_place_is_not_handed_to_the_recycler() {
    let env = Env::new();
    let docs = env.folder("users/me/docs/Mine");
    let out = process_job(&job(vec![env.item(0, &docs, Why::Leftover)]), &env.ctx());
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
    let out = process_job(&job(vec![env.item(0, &app, Why::Leftover), env.item(1, &inner_protected, Why::Leftover)]), &ctx);
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
    let out = process_job(&job(vec![env.item(0, &app, Why::Leftover)]), &ctx);
    assert!(reason(&out[0]).contains("settings file"));
    assert!(app.exists() && env.rec.calls.lock().unwrap().is_empty());
}

#[test]
fn the_engines_own_rules_apply_here_too() {
    let env = Env::new();
    // Two levels deep: the engine's scan-root rule for leftovers.
    let deep = env.folder("pf/App/Inner");
    // Claimed, as the engine's claims see it (not the same fake `check_item` was given).
    let claimed = env.folder("pf/Claimed");
    env.claims.0.lock().unwrap().insert(claimed.clone());
    let mut ctx = env.ctx();
    ctx.claims = env.claims.clone();
    let out = process_job(&job(vec![env.item(0, &deep, Why::Leftover), env.item(1, &claimed, Why::Leftover)]), &ctx);
    assert!(!is_moved(&out[0]) && !is_moved(&out[1]), "{out:?}");
    assert!(deep.exists() && claimed.exists());
    // Shared with Windows and other apps: an uninstall never offers anything in it.
    let shared = env.folder("pf/Common Files/Thing");
    let out = process_job(&job(vec![env.item(0, &shared, Why::Uninstall)]), &env.ctx());
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
    let out = process_job(&job(items), &env.ctx());
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
    let out = process_job(&job(vec![env.item(0, &app, Why::Leftover)]), &ctx);
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
    let out = process_job(&job(vec![env.item(0, &stale, Why::StaleData), env.item(1, &app, Why::Leftover)]), &ctx);
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
    let out = process_job(&job(vec![env.item(0, &first, Why::Leftover), env.item(1, &second, Why::Leftover)]), &env.ctx());
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
    let out = process_job(&job(vec![it.clone(), it.clone()]), &env.ctx());
    assert!(out.iter().all(|o| !is_moved(o)) && app.exists());
    let mut again = it.clone();
    again.id = 1;
    let out = process_job(&job(vec![it, again]), &env.ctx());
    assert!(is_moved(&out[0]) && !is_moved(&out[1]), "{out:?}");
}

// ---- job and result files -------------------------------------------------------------------------------

fn run_dir(base: &Path) -> PathBuf {
    let d = base.join(format!("{:032x}", 0xabcdefu128));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn refuse_everything_context(env: &Env) -> impl Fn(&ElevatedJob) -> Result<ElevatedContext, String> + '_ {
    |_| Ok(env.ctx())
}

#[test]
fn a_malformed_job_file_refuses_everything() {
    let env = Env::new();
    let app = env.folder("pf/App");
    let base = env.p("elevated");
    let dir = run_dir(&base);
    let file = dir.join(JOB_FILE);
    for bad in [&b"{ nope"[..], b"[]", b"", br#"{"userSid":"x","items":[{"id":"a"}]}"#] {
        std::fs::write(&file, bad).unwrap();
        let _ = std::fs::remove_file(dir.join(RESULT_FILE));
        let code = run_main_with(&file, &base, &refuse_everything_context(&env));
        assert_eq!(code, 2);
        let result: ElevatedResult = serde_json::from_slice(&std::fs::read(dir.join(RESULT_FILE)).unwrap()).unwrap();
        assert!(result.items.is_empty() && result.note.unwrap().contains("malformed"));
    }
    assert!(app.exists() && env.rec.calls.lock().unwrap().is_empty());
}

#[test]
fn a_job_over_the_caps_is_refused_unread() {
    let env = Env::new();
    let base = env.p("elevated");
    let dir = run_dir(&base);
    let file = dir.join(JOB_FILE);
    std::fs::write(&file, vec![b' '; (MAX_JOB_BYTES + 1) as usize]).unwrap();
    assert_eq!(run_main_with(&file, &base, &refuse_everything_context(&env)), 2);
    let items: Vec<JobItem> = (0..=MAX_JOB_ITEMS as u32).map(|id| JobItem { id, path: "x".into(), size: 0, modified_ms: 0, why: Why::Leftover }).collect();
    std::fs::write(&file, serde_json::to_vec(&job(items)).unwrap()).unwrap();
    assert!(std::fs::metadata(&file).unwrap().len() < MAX_JOB_BYTES);
    assert_eq!(run_main_with(&file, &base, &refuse_everything_context(&env)), 2);
    let result: ElevatedResult = serde_json::from_slice(&std::fs::read(dir.join(RESULT_FILE)).unwrap()).unwrap();
    assert!(result.note.unwrap().contains("too many"));
}

#[test]
fn a_job_file_outside_the_run_folder_layout_is_not_read() {
    let env = Env::new();
    let app = env.folder("pf/App");
    let base = env.p("elevated");
    let good = job(vec![env.item(0, &app, Why::Leftover)]);
    let bytes = serde_json::to_vec(&good).unwrap();
    let mut wrong: Vec<PathBuf> = Vec::new();
    // Not named job.json, a folder name that is not 128-bit hex, a folder deeper or shallower, another base.
    let d = run_dir(&base);
    wrong.push(d.join("other.json"));
    let short = base.join("abc");
    std::fs::create_dir_all(&short).unwrap();
    wrong.push(short.join(JOB_FILE));
    let upper = base.join("ABCDEFABCDEFABCDEFABCDEFABCDEFAB");
    std::fs::create_dir_all(&upper).unwrap();
    wrong.push(upper.join(JOB_FILE));
    wrong.push(base.join(JOB_FILE));
    let deeper = d.join("sub");
    std::fs::create_dir_all(&deeper).unwrap();
    wrong.push(deeper.join(JOB_FILE));
    let elsewhere = env.p("elsewhere").join(format!("{:032x}", 7u128));
    std::fs::create_dir_all(&elsewhere).unwrap();
    wrong.push(elsewhere.join(JOB_FILE));
    for f in &wrong {
        std::fs::write(f, &bytes).unwrap();
        assert_eq!(run_main_with(f, &base, &refuse_everything_context(&env)), 2, "{f:?}");
        assert!(!f.with_file_name(RESULT_FILE).exists(), "{f:?}");
    }
    assert!(app.exists() && env.rec.calls.lock().unwrap().is_empty());
    // A job that is a link to a file somewhere else is not read either.
    let real = d.join("real.json");
    std::fs::write(&real, &bytes).unwrap();
    let link = d.join(JOB_FILE);
    if std::os::windows::fs::symlink_file(&real, &link).is_ok() {
        assert_eq!(run_main_with(&link, &base, &refuse_everything_context(&env)), 2);
    }
    assert!(app.exists());
}

#[test]
fn a_good_job_is_processed_and_its_result_written_beside_it() {
    let env = Env::new();
    let app = env.folder("pf/App");
    let other = env.folder("users/me/docs/Mine");
    let base = env.p("elevated");
    let dir = run_dir(&base);
    let file = dir.join(JOB_FILE);
    let items = vec![env.item(0, &app, Why::Leftover), env.item(1, &other, Why::Leftover)];
    std::fs::write(&file, serde_json::to_vec(&job(items.clone())).unwrap()).unwrap();
    assert_eq!(run_main_with(&file, &base, &refuse_everything_context(&env)), 0);
    let bytes = std::fs::read(dir.join(RESULT_FILE)).unwrap();
    let back = match_results(&items, Some(&bytes));
    assert!(is_moved(&back[0]) && !is_moved(&back[1]));
    assert!(!app.exists() && other.exists());
    let tmp_left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".tmp")).collect();
    assert!(tmp_left.is_empty(), "{tmp_left:?}");
}

#[test]
fn a_context_that_cannot_be_built_refuses_every_item_with_its_reason() {
    let env = Env::new();
    let app = env.folder("pf/App");
    let base = env.p("elevated");
    let dir = run_dir(&base);
    let file = dir.join(JOB_FILE);
    let items = vec![env.item(0, &app, Why::Leftover)];
    std::fs::write(&file, serde_json::to_vec(&job(items.clone())).unwrap()).unwrap();
    assert_eq!(run_main_with(&file, &base, &|_| Err("Windows can't name its folders.".into())), 2);
    let back = match_results(&items, Some(&std::fs::read(dir.join(RESULT_FILE)).unwrap()));
    assert!(reason(&back[0]).contains("can't name"));
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
    serde_json::to_vec(&ElevatedResult { items, note: note.map(str::to_string) }).unwrap()
}

#[test]
fn results_are_matched_to_the_ids_that_were_sent() {
    let sent = sent(3);
    let moved = || Outcome::Moved { bin_path: "bin".into() };
    let bytes = result_bytes(vec![answer(2, moved()), answer(99, moved()), answer(0, Outcome::Left { reason: "in use".into() }), answer(0, moved())], None);
    let out = match_results(&sent, Some(&bytes));
    assert_eq!(out.len(), 3, "one outcome per item sent, none for id 99");
    assert_eq!(reason(&out[0]), "in use", "the first answer for an id wins");
    assert_eq!(reason(&out[1]), NO_REPORT);
    assert!(is_moved(&out[2]));
    assert_eq!((out[2].path.as_str(), out[2].size), (r"C:\Program Files\App2", 12), "path and size are what was sent");
}

#[test]
fn a_missing_malformed_or_partial_result_leaves_the_rest_in_place() {
    let sent = sent(2);
    for bytes in [None, Some(&b""[..]), Some(&b"{ half"[..]), Some(&b"[1,2]"[..]), Some(&br#"{"items":[{"id":0}]}"#[..])] {
        let out = match_results(&sent, bytes);
        assert!(out.iter().all(|o| reason(o) == NO_REPORT), "{bytes:?}");
    }
    let partial = result_bytes(vec![answer(1, Outcome::Moved { bin_path: "b".into() })], None);
    let out = match_results(&sent, Some(&partial));
    assert_eq!(reason(&out[0]), NO_REPORT);
    assert!(is_moved(&out[1]));
    // The elevated side's own reason for refusing the whole job is passed on.
    let noted = result_bytes(vec![], Some("The job file is malformed, so nothing was removed."));
    assert!(reason(&match_results(&sent, Some(&noted))[0]).contains("malformed"));
}

type Launched = Result<Option<Vec<u8>>, LaunchError>;

struct FakeLauncher {
    result: Result<Option<Vec<u8>>, LaunchError>,
    seen: Mutex<Vec<(PathBuf, String)>>,
    calls: Mutex<u32>,
}

impl FakeLauncher {
    fn new(result: Result<Option<Vec<u8>>, LaunchError>) -> FakeLauncher {
        FakeLauncher { result, seen: Mutex::default(), calls: Mutex::new(0) }
    }
}

impl Launcher for FakeLauncher {
    fn launch_and_wait(&self, job_file: &Path) -> Result<(), LaunchError> {
        *self.calls.lock().unwrap() += 1;
        self.seen.lock().unwrap().push((job_file.to_path_buf(), std::fs::read_to_string(job_file).unwrap()));
        match &self.result {
            Err(e) => Err(e.clone()),
            Ok(bytes) => {
                if let Some(b) = bytes {
                    std::fs::write(job_file.with_file_name(RESULT_FILE), b).unwrap();
                }
                Ok(())
            }
        }
    }
}

fn leftovers_of(base: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(base).map(|d| d.map(|e| e.unwrap().path()).collect()).unwrap_or_default()
}

#[test]
fn one_launch_sends_one_job_and_the_folder_is_deleted_afterwards_on_every_path() {
    let env = Env::new();
    let base = env.p("elevated");
    let items = sent(2);
    let good = result_bytes(vec![answer(0, Outcome::Moved { bin_path: "b".into() }), answer(1, Outcome::Moved { bin_path: "b".into() })], None);
    let cases: Vec<(Launched, &str)> = vec![
        (Ok(Some(good)), "moved"),
        (Ok(None), NO_REPORT),
        (Ok(Some(b"{ bad".to_vec())), NO_REPORT),
        (Err(LaunchError::Declined), DECLINED),
        (Err(LaunchError::Failed("The file is blocked".into())), "The file is blocked"),
    ];
    for (result, expect) in cases {
        let l = FakeLauncher::new(result);
        let out = run_elevated(&items, SID, 4242, &base, &l);
        assert_eq!(*l.calls.lock().unwrap(), 1);
        assert_eq!(out.len(), 2);
        for o in &out {
            match (&o.outcome, expect) {
                (Outcome::Moved { .. }, "moved") => {}
                (Outcome::Left { reason }, e) if e != "moved" => assert!(reason.contains(e), "{reason}"),
                other => panic!("{other:?} for {expect}"),
            }
        }
        assert!(leftovers_of(&base).is_empty(), "the run folder is gone after {expect}");
        let (job_file, text) = &l.seen.lock().unwrap()[0];
        assert!(job_file.starts_with(&base) && job_file.file_name().unwrap() == JOB_FILE);
        let hex = job_file.parent().unwrap().file_name().unwrap().to_string_lossy().into_owned();
        assert!(hex.len() == 32 && hex.chars().all(|c| c.is_ascii_hexdigit()), "{hex}");
        let sent_job: ElevatedJob = serde_json::from_str(text).unwrap();
        assert_eq!((sent_job.user_sid.as_str(), sent_job.window_pid, sent_job.items.len()), (SID, 4242, 2));
    }
}

#[test]
fn a_declined_prompt_says_so_for_each_item_and_a_failed_start_gives_the_error_text() {
    let env = Env::new();
    let base = env.p("elevated");
    let out = run_elevated(&sent(2), SID, 1, &base, &FakeLauncher::new(Err(LaunchError::Declined)));
    assert!(out.iter().all(|o| reason(o) == "Windows asked for admin and it was declined."));
    let out = run_elevated(&sent(1), SID, 1, &base, &FakeLauncher::new(Err(LaunchError::Failed("Access is denied. (os error 5)".into()))));
    assert!(reason(&out[0]).contains("Access is denied"));
}

#[test]
fn two_runs_never_share_a_folder() {
    let env = Env::new();
    let base = env.p("elevated");
    let a = FakeLauncher::new(Ok(None));
    run_elevated(&sent(1), SID, 1, &base, &a);
    run_elevated(&sent(1), SID, 1, &base, &a);
    let seen = a.seen.lock().unwrap();
    assert_ne!(seen[0].0, seen[1].0);
}

#[test]
fn a_run_folder_that_cannot_be_made_leaves_every_item_in_place() {
    let env = Env::new();
    let blocker = env.file("blocker", b"x"); // a file where the base folder should be
    let l = FakeLauncher::new(Ok(None));
    let out = run_elevated(&sent(2), SID, 1, &blocker.join("elevated"), &l);
    assert_eq!(*l.calls.lock().unwrap(), 0, "no prompt for a job that could not be written");
    assert!(out.iter().all(|o| reason(o).contains("couldn't prepare")));
}

#[test]
fn the_job_elevator_makes_one_launch_for_a_whole_batch() {
    let env = Env::new();
    let l = Arc::new(FakeLauncher::new(Err(LaunchError::Declined)));
    let e = JobElevator { launcher: l.clone(), base: Some(env.p("elevated")) };
    let out = e.run(&sent(3)).unwrap();
    assert_eq!(*l.calls.lock().unwrap(), 1);
    assert_eq!(out.len(), 3);
    let job: ElevatedJob = serde_json::from_str(&l.seen.lock().unwrap()[0].1).unwrap();
    assert_eq!(job.window_pid, std::process::id());
    assert_eq!(job.user_sid, current_user_sid().unwrap());
}
