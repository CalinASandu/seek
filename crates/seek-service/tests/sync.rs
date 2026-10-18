//! Linked PCs end to end: real services (pipe server, sync listener, sessions) over loopback, fake volumes.

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use seek_core::{
    Change, Checkpoint, DeviceIdentity, DeviceIndex, DeviceKind, DeviceState, RawEntry, SortKey, ATTR_DIRECTORY,
};
use seek_ipc::{Client, Request, Response};
use seek_scan::{Changed, DirWatcher, FollowError, ScanError, ScanResult, UsnFollower, VolumeInfo};
use seek_service::access::Caller;
use seek_service::peers::PeersRuntime;
use seek_service::{Config, State, SyncConfig, VolumeSource};
use seek_sync::peers::PeerState;
use tokio::net::windows::named_pipe::NamedPipeServer;

const ENTRIES: u64 = 1000;
const ME: &str = "S-1-5-21-me";

/// `ENTRIES` entries: folders `folder1..10`, files `<prefix><id>.txt` spread over them.
fn entries(prefix: &str) -> Vec<RawEntry> {
    (1..=ENTRIES)
        .map(|id| {
            let dir = id <= 10;
            RawEntry {
                id,
                parent_id: if dir { 0 } else { id % 10 + 1 },
                name: if dir { format!("folder{id}") } else { format!("{prefix}{id}.txt") },
                size: if dir { 0 } else { id },
                modified: 1_700_000_000_000,
                created: 1_700_000_000_000,
                attrs: if dir { ATTR_DIRECTORY } else { 0 },
            }
        })
        .collect()
}

fn volume(serial: u64) -> VolumeInfo {
    VolumeInfo {
        mount_point: "T:\\".into(),
        identity: DeviceIdentity::Volume { serial, filesystem: "exFAT".into() },
        label: "TESTSTICK".into(),
        filesystem: "exFAT".into(),
        kind: DeviceKind::Removable,
        total_bytes: 1 << 30,
        free_bytes: 1 << 29,
    }
}

type OnChange = Box<dyn Fn(Changed) + Send>;

struct Fake {
    vols: Mutex<Vec<VolumeInfo>>,
    prefix: String,
    on_change: Mutex<Option<OnChange>>,
    folder_changes: Mutex<Vec<Change>>,
    /// The pipe client is this user (SID), elevated or not; `profiles` = (SID, folder).
    sid: Mutex<String>,
    elevated: Mutex<bool>,
    profiles: Mutex<Vec<(String, String)>>,
}

impl Fake {
    fn new(prefix: &str, vols: Vec<VolumeInfo>) -> Arc<Fake> {
        Arc::new(Fake {
            vols: Mutex::new(vols),
            prefix: prefix.into(),
            on_change: Mutex::new(None),
            folder_changes: Mutex::new(Vec::new()),
            sid: Mutex::new(ME.into()),
            elevated: Mutex::new(false),
            profiles: Mutex::new(Vec::new()),
        })
    }
}

impl VolumeSource for Fake {
    fn list(&self) -> Vec<VolumeInfo> {
        self.vols.lock().unwrap().clone()
    }
    fn scan(&self, _v: &VolumeInfo, progress: &(dyn Fn(u64) + Sync)) -> Result<ScanResult, ScanError> {
        progress(ENTRIES);
        Ok(ScanResult { index: DeviceIndex::from_raw(0, entries(&self.prefix)), checkpoint: None, skipped: 0 })
    }
    fn follow(&self, _v: &VolumeInfo, _cp: Checkpoint) -> Result<UsnFollower, FollowError> {
        Err(FollowError::AccessDenied)
    }
    fn watch(&self, _v: &VolumeInfo, f: Box<dyn Fn(Changed) + Send>) -> Option<DirWatcher> {
        *self.on_change.lock().unwrap() = Some(f);
        None
    }
    fn rescan_dirs(
        &self,
        _v: &VolumeInfo,
        _ix: &DeviceIndex,
        _dirs: &[std::path::PathBuf],
    ) -> Result<Vec<Change>, ScanError> {
        Ok(std::mem::take(&mut *self.folder_changes.lock().unwrap()))
    }
    fn caller(&self, _pipe: &NamedPipeServer) -> Caller {
        Caller { elevated: *self.elevated.lock().unwrap(), sid: self.sid.lock().unwrap().clone() }
    }
    fn profiles(&self) -> Vec<(String, String)> {
        self.profiles.lock().unwrap().clone()
    }
}

struct Pc {
    pipe: String,
    state: Arc<State>,
    stop: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Pc {
    fn rt(&self) -> &Arc<PeersRuntime> {
        self.state.peers_rt.get().expect("sync is enabled")
    }
    fn id(&self) -> uuid::Uuid {
        self.state.host_id()
    }
    fn addr(&self) -> String {
        self.rt().local_addr().to_string()
    }
    fn peer_state(&self, other: &Pc) -> Option<PeerState> {
        self.state.peers.lock().unwrap().get(other.id()).map(|p| p.state)
    }
    fn connected_to(&self, other: &Pc) -> bool {
        self.state.connected.lock().unwrap().contains(&other.id())
    }
    async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
    /// A pipe client (the caller's SID/elevation are read from the fake when it connects).
    async fn client(&self) -> Client {
        for _ in 0..100 {
            if let Ok(c) = Client::connect_to(&self.pipe).await {
                return c;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("service did not start");
    }
}

async fn devices(c: &mut Client) -> Vec<seek_core::Device> {
    match c.request(Request::ListDevices).await.unwrap() {
        Response::Devices { devices } => devices,
        other => panic!("{other:?}"),
    }
}

async fn wait_devices(c: &mut Client, what: &str, pred: impl Fn(&[seek_core::Device]) -> bool) {
    for _ in 0..300 {
        if pred(&devices(c).await) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// `(search id, total)` of a search.
async fn search(c: &mut Client, q: &str) -> (u64, u64) {
    let r = c.request(Request::Search { query: q.into(), sort: SortKey::Name, descending: false }).await.unwrap();
    match r {
        Response::SearchStarted { search_id, total, .. } => (search_id, total),
        other => panic!("search {q:?}: {other:?}"),
    }
}

/// `guest` joins `host`'s group through the pipe API (as the app does).
async fn link_via_pipe(host: &Pc, guest: &Pc) {
    let code = match host.client().await.request(Request::StartPairing).await.unwrap() {
        Response::PairingCode { code, .. } => code,
        other => panic!("{other:?}"),
    };
    let r = guest.client().await.request(Request::JoinPairing { addr: host.addr(), code }).await.unwrap();
    assert_eq!(r, Response::Ok);
}

async fn start(dir: &Path, name: &str, fake: Arc<Fake>) -> Pc {
    let pipe = format!(r"\\.\pipe\seek-sync-test-{}", uuid::Uuid::new_v4());
    let (stop, rx) = tokio::sync::watch::channel(false);
    let cfg = Config {
        data_dir: dir.to_path_buf(),
        pipe_name: pipe.clone(),
        sync: Some(SyncConfig {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
            mdns: false,
            name: name.into(),
            interval: Duration::from_secs(1),
        }),
    };
    let (tx, ready) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(seek_service::run_with(cfg, fake, rx, Some(tx)));
    let state = ready.await.expect("service started");
    Pc { pipe, state, stop, task }
}

async fn wait(what: &str, pred: impl Fn() -> bool) {
    for _ in 0..1500 {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// `guest` joins `host`'s group.
async fn link(host: &Pc, guest: &Pc) {
    let code = host.rt().start_pairing(ME);
    guest.rt().join(&host.addr(), &code.code, ME).await.expect("pairing succeeds");
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_with_the_right_code_links_both_pcs() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = start(da.path(), "A", Fake::new("a", vec![])).await;
    let b = start(db.path(), "B", Fake::new("b", vec![])).await;

    let code = a.rt().start_pairing(ME);
    let name = b.rt().join(&a.addr(), &code.code, "S-1-5-21-me-on-b").await.unwrap();
    assert_eq!(name, "A");

    wait("both linked", || a.peer_state(&b) == Some(PeerState::Linked) && b.peer_state(&a) == Some(PeerState::Linked))
        .await;
    {
        let peers = a.state.peers.lock().unwrap();
        let pb = peers.get(b.id()).unwrap();
        assert_eq!(pb.name, "B");
        assert_eq!(pb.linked_users, vec![(ME.to_string(), "S-1-5-21-me-on-b".to_string())]);
    }
    assert_eq!(
        b.state.peers.lock().unwrap().get(a.id()).unwrap().linked_users,
        vec![("S-1-5-21-me-on-b".to_string(), ME.to_string())]
    );
    wait("a session", || a.connected_to(&b) && b.connected_to(&a)).await;
    // the code was used up
    assert!(b.rt().join(&a.addr(), &code.code, ME).await.is_err());
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_code_three_times_expires_the_code() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = start(da.path(), "A", Fake::new("a", vec![])).await;
    let b = start(db.path(), "B", Fake::new("b", vec![])).await;

    let code = a.rt().start_pairing(ME);
    let wrong = if code.code == "000000" { "111111" } else { "000000" };
    for _ in 0..3 {
        let e = b.rt().join(&a.addr(), wrong, ME).await.unwrap_err();
        assert_eq!(e, "wrong code");
    }
    let e = b.rt().join(&a.addr(), &code.code, ME).await.unwrap_err();
    assert!(e.contains("not showing a code"), "{e}");
    assert!(a.state.peers.lock().unwrap().file.peers.is_empty());
    assert!(b.state.peers.lock().unwrap().file.peers.is_empty());
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reinstalled_peer_needs_relinking() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = start(da.path(), "A", Fake::new("a", vec![])).await;
    let b = start(db.path(), "B", Fake::new("b", vec![])).await;
    link(&a, &b).await;
    wait("a session", || a.connected_to(&b)).await;

    let b_id = b.id();
    b.stop().await;
    wait("session gone", || !a.state.connected.lock().unwrap().contains(&b_id)).await;
    // reinstall: new key, same host id (devices.json kept) and still remembers A
    std::fs::remove_file(db.path().join("host.key")).unwrap();
    let b = start(db.path(), "B", Fake::new("b", vec![])).await;
    assert_eq!(b.id(), b_id);

    wait("A refuses B", || a.peer_state(&b) == Some(PeerState::NeedsRelink)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!a.connected_to(&b), "a reinstalled PC is never silently trusted again");
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn third_pc_joining_through_b_is_linked_with_a() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = start(dirs[0].path(), "A", Fake::new("a", vec![])).await;
    let b = start(dirs[1].path(), "B", Fake::new("b", vec![])).await;
    let c = start(dirs[2].path(), "C", Fake::new("c", vec![])).await;
    link(&a, &b).await;
    wait("A–B session", || a.connected_to(&b) && b.connected_to(&a)).await;

    link(&b, &c).await;
    wait("A knows C", || a.peer_state(&c) == Some(PeerState::Linked)).await;
    assert_eq!(c.peer_state(&a), Some(PeerState::Linked));
    wait("A–C session", || a.connected_to(&c) && c.connected_to(&a)).await;
    // user links exist only between the two PCs where the code was typed
    assert!(a.state.peers.lock().unwrap().get(c.id()).unwrap().linked_users.is_empty());
    for pc in [a, b, c] {
        pc.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn group_is_capped_at_five_pcs() {
    let dirs: Vec<_> = (0..6).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut pcs = Vec::new();
    for (i, d) in dirs.iter().enumerate() {
        let name = format!("PC{i}");
        pcs.push(start(d.path(), &name, Fake::new(&name, vec![])).await);
    }
    for guest in &pcs[1..5] {
        link(&pcs[0], guest).await;
    }
    assert_eq!(pcs[0].state.peers.lock().unwrap().file.peers.len(), 4);

    let code = pcs[0].rt().start_pairing(ME);
    let e = pcs[5].rt().join(&pcs[0].addr(), &code.code, ME).await.unwrap_err();
    assert!(e.contains("full"), "{e}");
    assert!(pcs[5].state.peers.lock().unwrap().file.peers.is_empty());
    for pc in pcs {
        pc.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unlink_removes_the_link_and_copies_on_both_sides() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = start(da.path(), "A", Fake::new("a", vec![volume(1)])).await;
    let b = start(db.path(), "B", Fake::new("b", vec![volume(2)])).await;
    let held = |pc: &Pc, owner: &Pc| {
        pc.state.registry.read().unwrap().devices.iter().filter(|d| d.indexed_by_host == owner.id()).count()
    };
    link(&a, &b).await;
    wait("copies on both sides", || {
        held(&a, &b) == 1 && held(&b, &a) == 1 && a.state.indexes.read().unwrap().len() == 2
            && b.state.indexes.read().unwrap().len() == 2
    })
    .await;

    a.rt().unlink(b.id()).await.unwrap();
    wait("unlinked on both sides", || {
        a.peer_state(&b).is_none() && b.peer_state(&a).is_none() && held(&a, &b) == 0 && held(&b, &a) == 0
    })
    .await;
    assert_eq!(a.state.indexes.read().unwrap().len(), 1);
    assert_eq!(b.state.indexes.read().unwrap().len(), 1);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!a.connected_to(&b) && !b.connected_to(&a), "no session after unlinking");
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pc_that_was_off_during_unlink_learns_it_when_it_comes_back() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = start(da.path(), "A", Fake::new("a", vec![])).await;
    let b = start(db.path(), "B", Fake::new("b", vec![])).await;
    link(&a, &b).await;
    wait("a session", || a.connected_to(&b)).await;
    let b_id = b.id();
    b.stop().await;
    wait("session gone", || !a.state.connected.lock().unwrap().contains(&b_id)).await;

    a.rt().unlink(b_id).await.unwrap();
    let b = start(db.path(), "B", Fake::new("b", vec![])).await;
    assert_eq!(b.peer_state(&a), Some(PeerState::Linked), "B still remembers A after the restart");
    wait("B told about the unlink", || b.peer_state(&a).is_none()).await;
    assert!(a.peer_state(&b).is_none());
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn linked_pcs_search_each_others_files() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let fa = Fake::new("file", vec![volume(0xABCD)]);
    let a = start(da.path(), "A", fa.clone()).await;
    let b = start(db.path(), "B", Fake::new("b", vec![volume(0x1234)])).await;
    let (mut ca, mut cb) = (a.client().await, b.client().await);
    let a_id = a.id();
    wait_devices(&mut ca, "A scanned", |ds| ds.iter().any(|d| d.entry_count == ENTRIES)).await;
    let a_dev = devices(&mut ca).await.into_iter().find(|d| d.indexed_by_host == a_id).unwrap().id;

    link_via_pipe(&a, &b).await;
    wait_devices(&mut cb, "B holds A's device", |ds| {
        ds.iter().any(|d| d.id == a_dev && d.indexed_by_host == a_id && d.entry_count == ENTRIES)
    })
    .await;

    let (_, local) = search(&mut ca, "file5 pc:A").await;
    assert!(local > 0);
    let (sid, remote) = search(&mut cb, "file5").await;
    assert_eq!(remote, local, "B finds A's files (its own are named b*.txt)");
    match cb.request(Request::FetchRows { search_id: sid, offset: 0, limit: 10 }).await.unwrap() {
        Response::Rows { rows, .. } => {
            assert!(!rows.is_empty());
            assert!(rows.iter().all(|r| r.device_id == a_dev));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(search(&mut cb, "pc:A file5").await.1, local);
    assert_eq!(search(&mut cb, "pc:B file5").await.1, 0);
    assert_eq!(search(&mut cb, "device:a file5").await.1, local, "device: also matches the PC name");

    // a change on A reaches B
    *fa.folder_changes.lock().unwrap() = vec![Change::Upsert(RawEntry {
        id: 5000,
        parent_id: 1,
        name: "zzz_new.txt".into(),
        size: 7,
        modified: 1_700_000_000_000,
        created: 1_700_000_000_000,
        attrs: 0,
    })];
    {
        let on_change = fa.on_change.lock().unwrap();
        let folder = std::path::PathBuf::from(r"T:\folder1");
        on_change.as_ref().expect("A watches its volume")(Changed::Dirs([folder].into_iter().collect()));
    }
    let mut found = 0;
    for _ in 0..50 {
        assert_eq!(cb.request(Request::SyncNow).await.unwrap(), Response::Ok);
        found = search(&mut cb, "zzz_new").await.1;
        if found == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(found, 1, "the new file on A is searchable on B within 5 s");

    // A goes away: B still searches its copy, now offline
    a.stop().await;
    wait_devices(&mut cb, "A's device offline", |ds| {
        ds.iter().any(|d| d.id == a_dev && d.state == DeviceState::Offline)
    })
    .await;
    assert_eq!(search(&mut cb, "file5").await.1, local);
    match cb.request(Request::ListPeers).await.unwrap() {
        Response::Peers { peers, this_pc, .. } => {
            assert_eq!(this_pc.name, "B");
            assert_eq!(peers.len(), 1);
            assert!(!peers[0].online && peers[0].last_sync_ms > 0);
        }
        other => panic!("{other:?}"),
    }
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn other_users_folders_on_a_remote_pc_are_hidden_unless_linked_or_elevated() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let fa = Fake::new("file", vec![volume(0xABCD)]);
    *fa.profiles.lock().unwrap() =
        vec![(ME.into(), r"T:\folder1".into()), ("S-1-5-21-bob".into(), r"T:\folder2".into())];
    let fb = Fake::new("b", vec![volume(0x1234)]);
    *fb.sid.lock().unwrap() = "S-1-5-21-me-on-b".into();
    let a = start(da.path(), "A", fa).await;
    let b = start(db.path(), "B", fb.clone()).await;
    let a_id = a.id();
    link_via_pipe(&a, &b).await; // A's code shown to ME, typed on B by me-on-b: the same person
    let mut cb = b.client().await;
    wait_devices(&mut cb, "B holds A's device", |ds| {
        ds.iter().any(|d| d.indexed_by_host == a_id && d.entry_count == ENTRIES)
    })
    .await;

    // me on A sees everything but bob's folder; so does me on B
    let on_a = search(&mut a.client().await, "pc:A").await.1;
    assert!(on_a < ENTRIES, "bob's folder is hidden on A");
    assert_eq!(search(&mut cb, "pc:A").await.1, on_a);
    assert_eq!(search(&mut cb, "pc:A path:folder2").await.1, 0);
    assert!(search(&mut cb, "pc:A path:folder1").await.1 > 0);

    // someone else on B is not linked with anyone on A: both profile folders are hidden
    *fb.sid.lock().unwrap() = "S-1-5-21-stranger".into();
    let mut stranger = b.client().await;
    let seen = search(&mut stranger, "pc:A").await.1;
    assert!(seen < on_a, "{seen} < {on_a}");
    // (`path:folder1` also matches the unrelated folder10, which nobody owns)
    let folder10 = search(&mut stranger, "pc:A path:folder10").await.1;
    assert!(folder10 > 0);
    assert_eq!(search(&mut stranger, "pc:A path:folder1").await.1, folder10);

    // an elevated caller sees everything
    *fb.elevated.lock().unwrap() = true;
    assert_eq!(search(&mut b.client().await, "pc:A").await.1, ENTRIES);
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn same_usb_stick_on_both_pcs_shows_once() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = start(da.path(), "A", Fake::new("file", vec![volume(0xABCD)])).await;
    let fb = Fake::new("file", vec![volume(0xABCD)]);
    let b = start(db.path(), "B", fb.clone()).await;
    let (a_id, b_id) = (a.id(), b.id());
    let (mut ca, mut cb) = (a.client().await, b.client().await);
    wait_devices(&mut ca, "A scanned", |ds| ds.iter().any(|d| d.entry_count == ENTRIES)).await;
    wait_devices(&mut cb, "B scanned", |ds| ds.iter().any(|d| d.entry_count == ENTRIES)).await;
    link_via_pipe(&a, &b).await;
    wait("B holds A's copy", || {
        b.state.registry.read().unwrap().devices.iter().any(|d| d.indexed_by_host == a_id && d.entry_count == ENTRIES)
    })
    .await;

    let stick = volume(0xABCD).identity;
    let shown: Vec<_> = devices(&mut cb).await.into_iter().filter(|d| d.identity == stick).collect();
    assert_eq!(shown.len(), 1, "one physical stick, one entry");
    assert_eq!((shown[0].indexed_by_host, shown[0].state), (b_id, DeviceState::Online), "the plugged-in one");
    let (_, t) = search(&mut cb, "file5").await;
    let (_, t_a) = search(&mut ca, "file5 pc:A").await;
    assert_eq!(t, t_a, "results are not doubled");

    // unplugged on B, then rescanned on A: A's copy is the newer one
    fb.vols.lock().unwrap().clear();
    wait("B's stick offline", || {
        b.state.registry.read().unwrap().devices.iter().all(|d| d.indexed_by_host != b_id || d.state == DeviceState::Offline)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let a_dev = devices(&mut ca).await.into_iter().find(|d| d.identity == stick).unwrap().id;
    assert_eq!(ca.request(Request::RescanDevice { id: a_dev }).await.unwrap(), Response::Ok);
    wait_devices(&mut cb, "B shows A's copy", |ds| {
        let shown: Vec<_> = ds.iter().filter(|d| d.identity == stick).collect();
        shown.len() == 1 && shown[0].id == a_dev
    })
    .await;
    // a copy of another PC's device cannot be rescanned or forgotten from here
    let r = cb.request(Request::RescanDevice { id: a_dev }).await.unwrap();
    assert_eq!(r, Response::Error { message: "this drive belongs to A".into() });
    a.stop().await;
    b.stop().await;
}
