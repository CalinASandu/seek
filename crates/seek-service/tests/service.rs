//! End-to-end tests: the real service loop and pipe server, with a fake volume source.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use seek_core::{
    encode_index, Checkpoint, DeviceIdentity, DeviceIndex, DeviceKind, DeviceState, IndexHeader,
    RawEntry, SortKey, ATTR_DIRECTORY,
};
use seek_ipc::{Client, MuxClient, Request, Response};
use seek_scan::{DirWatcher, FollowError, ScanError, ScanResult, UsnFollower, VolumeInfo};
use seek_service::{Config, VolumeSource};

const ENTRIES: u64 = 1000;

fn entries() -> Vec<RawEntry> {
    (1..=ENTRIES)
        .map(|id| {
            let dir = id <= 10;
            RawEntry {
                id,
                parent_id: if dir { 0 } else { id % 10 + 1 },
                name: if dir { format!("folder{id}") } else { format!("file{id}.txt") },
                size: if dir { 0 } else { id },
                modified: 1_700_000_000_000,
                created: 1_700_000_000_000,
                attrs: if dir { ATTR_DIRECTORY } else { 0 },
            }
        })
        .collect()
}

fn volume() -> VolumeInfo {
    VolumeInfo {
        mount_point: "T:\\".into(),
        identity: DeviceIdentity::Volume { serial: 0xABCD, filesystem: "exFAT".into() },
        label: "TESTSTICK".into(),
        filesystem: "exFAT".into(),
        kind: DeviceKind::Removable,
        total_bytes: 1 << 30,
        free_bytes: 1 << 29,
    }
}

type OnChange = Box<dyn Fn() + Send>;

#[derive(Default)]
struct Fake {
    vols: Mutex<Vec<VolumeInfo>>,
    scans: Mutex<u32>,
    /// The callback the service registered for "volume changed" (no real watcher is created).
    on_change: Mutex<Option<OnChange>>,
}

impl VolumeSource for Fake {
    fn list(&self) -> Vec<VolumeInfo> {
        self.vols.lock().unwrap().clone()
    }
    fn scan(&self, _v: &VolumeInfo, progress: &(dyn Fn(u64) + Sync)) -> Result<ScanResult, ScanError> {
        *self.scans.lock().unwrap() += 1;
        progress(ENTRIES);
        Ok(ScanResult { index: DeviceIndex::from_raw(0, entries()), checkpoint: None, skipped: 0 })
    }
    fn follow(&self, _v: &VolumeInfo, _cp: Checkpoint) -> Result<UsnFollower, FollowError> {
        Err(FollowError::AccessDenied)
    }
    fn watch(&self, _v: &VolumeInfo, f: Box<dyn Fn() + Send>) -> Option<DirWatcher> {
        *self.on_change.lock().unwrap() = Some(f);
        None
    }
}

struct Running {
    pipe: String,
    stop: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

async fn start(dir: &Path, fake: Arc<Fake>) -> Running {
    let pipe = format!(r"\\.\pipe\seek-test-{}", uuid::Uuid::new_v4());
    let (stop, rx) = tokio::sync::watch::channel(false);
    let cfg = Config { data_dir: dir.to_path_buf(), pipe_name: pipe.clone() };
    let task = tokio::spawn(seek_service::run(cfg, fake, rx));
    Running { pipe, stop, task }
}

async fn connect(pipe: &str) -> Client {
    for _ in 0..100 {
        if let Ok(c) = Client::connect_to(pipe).await {
            return c;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("service did not start");
}

async fn wait_for(c: &mut Client, what: &str, pred: impl Fn(&[seek_core::Device]) -> bool) {
    for _ in 0..200 {
        if let Response::Devices { devices } = c.request(Request::ListDevices).await.unwrap() {
            if pred(&devices) {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for: {what}");
}

async fn search(c: &mut Client, q: &str) -> Response {
    c.request(Request::Search { query: q.into(), sort: SortKey::Name, descending: false }).await.unwrap()
}

fn total(r: Response) -> (u64, u64) {
    match r {
        Response::SearchStarted { search_id, total, .. } => (search_id, total),
        other => panic!("expected SearchStarted, got {other:?}"),
    }
}

fn scanned_online(ds: &[seek_core::Device]) -> bool {
    ds.len() == 1 && ds[0].state == DeviceState::Online && ds[0].entry_count == ENTRIES
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_search_and_paging() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.vols.lock().unwrap().push(volume());
    let svc = start(dir.path(), fake).await;
    let mut c = connect(&svc.pipe).await;

    let hello = c.request(Request::Hello { protocol_version: 1 }).await.unwrap();
    assert!(matches!(hello, Response::Hello { protocol_version: 1, .. }));
    let bad = c.request(Request::Hello { protocol_version: 99 }).await.unwrap();
    assert!(matches!(bad, Response::Error { .. }));

    wait_for(&mut c, "device scanned", scanned_online).await;

    let (id, n) = total(search(&mut c, "").await);
    assert_eq!(n, ENTRIES);
    let rows = |r| match r {
        Response::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    };
    let page = rows(c.request(Request::FetchRows { search_id: id, offset: 0, limit: 50 }).await.unwrap());
    assert_eq!(page.len(), 50);
    assert!(page.iter().all(|r| r.online && r.device_name == "TESTSTICK (T:)"));
    let past = rows(c.request(Request::FetchRows { search_id: id, offset: 5000, limit: 50 }).await.unwrap());
    assert!(past.is_empty());
    let capped = rows(c.request(Request::FetchRows { search_id: id, offset: 0, limit: 10_000 }).await.unwrap());
    assert_eq!(capped.len(), 500);

    let (id2, n2) = total(search(&mut c, "file99.txt").await);
    assert_eq!(n2, 1);
    let hit = rows(c.request(Request::FetchRows { search_id: id2, offset: 0, limit: 10 }).await.unwrap());
    assert_eq!(hit[0].full_path, "T:\\folder10\\file99.txt");

    match search(&mut c, "ext:").await {
        Response::SearchError { position, .. } => assert_eq!(position, 0),
        other => panic!("expected SearchError, got {other:?}"),
    }
    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn unplugged_device_stays_searchable_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.vols.lock().unwrap().push(volume());
    let svc = start(dir.path(), fake.clone()).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "device scanned", scanned_online).await;

    fake.vols.lock().unwrap().clear(); // yank the stick
    wait_for(&mut c, "device offline", |ds| ds[0].state == DeviceState::Offline).await;
    assert_eq!(total(search(&mut c, "offline:").await).1, ENTRIES);
    assert_eq!(total(search(&mut c, "online:").await).1, 0);

    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();

    // Restart with the device still absent: the stored index is loaded, no scan happens.
    let fake2 = Arc::new(Fake::default());
    let svc = start(dir.path(), fake2.clone()).await;
    let mut c = connect(&svc.pipe).await;
    assert_eq!(total(search(&mut c, "file").await).1, ENTRIES - 10);
    assert_eq!(*fake2.scans.lock().unwrap(), 0);
    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn corrupt_index_file_is_rescanned() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.vols.lock().unwrap().push(volume());
    let svc = start(dir.path(), fake.clone()).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "device scanned", scanned_online).await;
    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();

    let idx_dir = dir.path().join("indexes");
    for f in std::fs::read_dir(&idx_dir).unwrap() {
        std::fs::write(f.unwrap().path(), b"SEEKIDX\0garbage").unwrap();
    }
    let svc = start(dir.path(), fake.clone()).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "device rescanned", scanned_online).await;
    assert_eq!(total(search(&mut c, "").await).1, ENTRIES);
    assert_eq!(*fake.scans.lock().unwrap(), 2);
    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_forget_and_network_share() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.vols.lock().unwrap().push(volume());
    let svc = start(dir.path(), fake.clone()).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "device scanned", scanned_online).await;
    let Response::Devices { devices } = c.request(Request::ListDevices).await.unwrap() else { panic!() };
    let id = devices[0].id;

    let r = c.request(Request::RenameDevice { id, name: "Holiday stick".into() }).await.unwrap();
    assert_eq!(r, Response::Ok);
    wait_for(&mut c, "renamed", |ds| ds[0].display_name == "Holiday stick").await;
    let r = c.request(Request::ForgetDevice { id }).await.unwrap();
    assert!(matches!(r, Response::Error { .. }), "online device must not be forgettable");

    // Network share: register, push an index built by the "app", search it.
    let Response::Device { device: share } =
        c.request(Request::AddNetworkShare { unc: "\\\\NAS\\Media\\".into() }).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(share.identity, DeviceIdentity::Share { unc: "\\\\nas\\media".into() });
    assert_eq!(share.display_name, "media");
    let bad = c.request(Request::AddNetworkShare { unc: "nas".into() }).await.unwrap();
    assert!(matches!(bad, Response::Error { .. }));

    let mut ix = DeviceIndex::from_raw(
        0,
        vec![RawEntry { id: 1, parent_id: 0, name: "movie.mkv".into(), size: 5, modified: 0, created: 0, attrs: 0 }],
    );
    let hdr = IndexHeader {
        format_version: 1,
        device_id: share.id,
        host_id: uuid::Uuid::nil(),
        checkpoint: None,
        saved_at: 0,
        rows: 0,
    };
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(encode_index(&hdr, &mut ix).unwrap());
    let r = c.request(Request::PushShareIndex { device_id: share.id, data_b64 }).await.unwrap();
    assert_eq!(r, Response::Ok);
    let (sid, n) = total(search(&mut c, "movie").await);
    assert_eq!(n, 1);
    let Response::Rows { rows, .. } = c.request(Request::FetchRows { search_id: sid, offset: 0, limit: 5 }).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(rows[0].full_path, "\\\\nas\\media\\movie.mkv");

    // Replacing the share's index renumbers its rows: the old result set must refuse to serve them
    // rather than return whatever file now sits at the same row number.
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(encode_index(&hdr, &mut ix).unwrap());
    assert_eq!(c.request(Request::PushShareIndex { device_id: share.id, data_b64 }).await.unwrap(), Response::Ok);
    let stale = c.request(Request::FetchRows { search_id: sid, offset: 0, limit: 5 }).await.unwrap();
    assert!(matches!(stale, Response::Error { .. }), "got {stale:?}");
    let (sid2, _) = total(search(&mut c, "movie").await);
    assert!(sid2 > sid, "search ids keep increasing");

    // Unplug, then forget the stick: its index file is removed.
    fake.vols.lock().unwrap().clear();
    wait_for(&mut c, "offline", |ds| ds.iter().any(|d| d.id == id && d.state == DeviceState::Offline)).await;
    assert_eq!(c.request(Request::ForgetDevice { id }).await.unwrap(), Response::Ok);
    assert!(!dir.path().join("indexes").join(format!("{id}.seekidx")).exists());
    assert_eq!(total(search(&mut c, "file").await).1, 0);

    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn watched_volume_changes_trigger_one_rate_limited_rescan() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.vols.lock().unwrap().push(volume());
    let svc = start(dir.path(), fake.clone()).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "device scanned", scanned_online).await;
    assert_eq!(*fake.scans.lock().unwrap(), 1);

    // A burst of change notifications collapses into a single rescan after the cooldown (10 s minimum).
    {
        let cb = fake.on_change.lock().unwrap();
        let cb = cb.as_ref().expect("a watcher was attached after the scan");
        cb();
        cb();
        cb();
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(*fake.scans.lock().unwrap(), 1, "rescan must wait for the cooldown");
    for _ in 0..200 {
        if *fake.scans.lock().unwrap() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(*fake.scans.lock().unwrap(), 2);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(*fake.scans.lock().unwrap(), 2, "no further scans without new changes");

    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn mux_client_routes_concurrent_responses() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.vols.lock().unwrap().push(volume());
    let svc = start(dir.path(), fake).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "device scanned", scanned_online).await;

    let mux = Arc::new(MuxClient::new(connect(&svc.pipe).await));
    let tasks: Vec<_> = (0..20u64)
        .map(|i| {
            let mux = mux.clone();
            tokio::spawn(async move {
                if i % 2 == 0 {
                    let r = mux.request(Request::ListDevices).await.unwrap();
                    assert!(matches!(r, Response::Devices { .. }), "{r:?}");
                } else {
                    let q = format!("file{i}.txt");
                    let r = mux
                        .request(Request::Search { query: q, sort: SortKey::Name, descending: false })
                        .await
                        .unwrap();
                    // a newer search on the same connection may cancel an older one
                    assert!(matches!(r, Response::SearchStarted { .. } | Response::Error { .. }), "{r:?}");
                }
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
}
