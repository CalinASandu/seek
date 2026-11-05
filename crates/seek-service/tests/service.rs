//! End-to-end tests: the real service loop and pipe server, with a fake volume source.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use seek_core::{
    encode_index, Change, Checkpoint, DeviceIdentity, DeviceIndex, DeviceKind, DeviceState,
    IndexHeader, RawEntry, SortKey, ATTR_DIRECTORY,
};
use seek_ipc::{Client, MuxClient, Request, Response, UserVolume, PROTOCOL_VERSION};
use seek_scan::{Changed, DirWatcher, FollowError, ScanError, ScanResult, UsnFollower, VolumeInfo};
use seek_service::access::Caller;
use seek_service::{Config, VolumeSource};
use tokio::net::windows::named_pipe::NamedPipeServer;

const ENTRIES: u64 = 1000;

/// `size_mod` > 0 makes file sizes repeat: size = id % size_mod (instead of every size being unique).
fn entries_sized(size_mod: u64) -> Vec<RawEntry> {
    (1..=ENTRIES)
        .map(|id| {
            let dir = id <= 10;
            RawEntry {
                id,
                parent_id: if dir { 0 } else { id % 10 + 1 },
                name: if dir {
                    format!("folder{id}")
                } else {
                    format!("file{id}.txt")
                },
                size: match (dir, size_mod) {
                    (true, _) => 0,
                    (false, 0) => id,
                    (false, m) => id % m,
                },
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
        identity: DeviceIdentity::Volume {
            serial: 0xABCD,
            filesystem: "exFAT".into(),
        },
        label: "TESTSTICK".into(),
        filesystem: "exFAT".into(),
        kind: DeviceKind::Removable,
        total_bytes: 1 << 30,
        free_bytes: 1 << 29,
    }
}

type OnChange = Box<dyn Fn(Changed) + Send>;

#[derive(Default)]
struct Fake {
    vols: Mutex<Vec<VolumeInfo>>,
    scans: Mutex<u32>,
    /// The callback the service registered for "volume changed" (no real watcher is created).
    on_change: Mutex<Option<OnChange>>,
    /// Folders the service asked to rescan, and the changes to answer with.
    rescanned: Mutex<Vec<std::path::PathBuf>>,
    folder_changes: Mutex<Vec<Change>>,
    /// Every client is this non-elevated user unless `elevated`; `profiles` = (SID, folder).
    elevated: Mutex<bool>,
    profiles: Mutex<Vec<(String, String)>>,
    /// See `entries_sized`.
    size_mod: Mutex<u64>,
}

const ME: &str = "S-1-5-21-me";

impl VolumeSource for Fake {
    fn list(&self) -> Vec<VolumeInfo> {
        self.vols.lock().unwrap().clone()
    }
    fn scan(
        &self,
        _v: &VolumeInfo,
        progress: &(dyn Fn(u64) + Sync),
    ) -> Result<ScanResult, ScanError> {
        *self.scans.lock().unwrap() += 1;
        progress(ENTRIES);
        Ok(ScanResult {
            index: DeviceIndex::from_raw(0, entries_sized(*self.size_mod.lock().unwrap())),
            checkpoint: None,
            skipped: 0,
        })
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
        dirs: &[std::path::PathBuf],
    ) -> Result<Vec<Change>, ScanError> {
        self.rescanned.lock().unwrap().extend(dirs.iter().cloned());
        Ok(std::mem::take(&mut *self.folder_changes.lock().unwrap()))
    }
    fn caller(&self, _pipe: &NamedPipeServer) -> Caller {
        Caller {
            elevated: *self.elevated.lock().unwrap(),
            sid: ME.into(),
        }
    }
    fn profiles(&self) -> Vec<(String, String)> {
        self.profiles.lock().unwrap().clone()
    }
}

struct Running {
    pipe: String,
    stop: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

async fn start(dir: &Path, fake: Arc<Fake>) -> Running {
    start_idle(dir, fake, None).await
}

async fn start_idle(dir: &Path, fake: Arc<Fake>, idle_unload: Option<Duration>) -> Running {
    let pipe = format!(r"\\.\pipe\seek-test-{}", uuid::Uuid::new_v4());
    let (stop, rx) = tokio::sync::watch::channel(false);
    let cfg = Config {
        data_dir: dir.to_path_buf(),
        pipe_name: pipe.clone(),
        sync: None,
        idle_unload,
    };
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
    c.request(Request::Search {
        query: q.into(),
        sort: SortKey::Name,
        descending: false,
    })
    .await
    .unwrap()
}

fn total(r: Response) -> (u64, u64) {
    match r {
        Response::SearchStarted {
            search_id, total, ..
        } => (search_id, total),
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

    let hello = c
        .request(Request::Hello {
            protocol_version: PROTOCOL_VERSION,
        })
        .await
        .unwrap();
    assert!(matches!(
        hello,
        Response::Hello {
            protocol_version: PROTOCOL_VERSION,
            ..
        }
    ));
    let bad = c
        .request(Request::Hello {
            protocol_version: 99,
        })
        .await
        .unwrap();
    assert!(matches!(bad, Response::Error { .. }));

    wait_for(&mut c, "device scanned", scanned_online).await;

    let (id, n) = total(search(&mut c, "").await);
    assert_eq!(n, ENTRIES);
    let rows = |r| match r {
        Response::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    };
    let page = rows(
        c.request(Request::FetchRows {
            search_id: id,
            offset: 0,
            limit: 50,
        })
        .await
        .unwrap(),
    );
    assert_eq!(page.len(), 50);
    assert!(page
        .iter()
        .all(|r| r.online && r.device_name == "TESTSTICK (T:)"));
    let past = rows(
        c.request(Request::FetchRows {
            search_id: id,
            offset: 5000,
            limit: 50,
        })
        .await
        .unwrap(),
    );
    assert!(past.is_empty());
    let capped = rows(
        c.request(Request::FetchRows {
            search_id: id,
            offset: 0,
            limit: 10_000,
        })
        .await
        .unwrap(),
    );
    assert_eq!(capped.len(), 500);

    let (id2, n2) = total(search(&mut c, "file99.txt").await);
    assert_eq!(n2, 1);
    let hit = rows(
        c.request(Request::FetchRows {
            search_id: id2,
            offset: 0,
            limit: 10,
        })
        .await
        .unwrap(),
    );
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
    wait_for(&mut c, "device offline", |ds| {
        ds[0].state == DeviceState::Offline
    })
    .await;
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
async fn idle_indexes_unload_and_come_back_for_the_next_search() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.vols.lock().unwrap().push(volume());
    let svc = start_idle(dir.path(), fake.clone(), Some(Duration::from_millis(1))).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "device scanned", scanned_online).await;
    let (id, found) = total(search(&mut c, "file").await);

    // the stored results go with the index, and the client is told to search again
    let mut gone = false;
    for _ in 0..240 {
        let r = c.request(Request::FetchRows { search_id: id, offset: 0, limit: 1 }).await.unwrap();
        if let Response::Error { message } = r {
            assert!(message.contains("results changed"), "{message}");
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone, "the index was never unloaded");

    assert_eq!(total(search(&mut c, "file").await).1, found);
    assert!(matches!(c.request(Request::Wake).await.unwrap(), Response::Ok));
    assert_eq!(*fake.scans.lock().unwrap(), 1, "reloading is not rescanning");
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
    let Response::Devices { devices } = c.request(Request::ListDevices).await.unwrap() else {
        panic!()
    };
    let id = devices[0].id;

    let r = c
        .request(Request::RenameDevice {
            id,
            name: "Holiday stick".into(),
        })
        .await
        .unwrap();
    assert_eq!(r, Response::Ok);
    wait_for(&mut c, "renamed", |ds| {
        ds[0].display_name == "Holiday stick"
    })
    .await;
    let r = c.request(Request::ForgetDevice { id }).await.unwrap();
    assert!(
        matches!(r, Response::Error { .. }),
        "online device must not be forgettable"
    );

    // Network share: register, push an index built by the "app", search it.
    let Response::Device { device: share } = c
        .request(Request::AddNetworkShare {
            unc: "\\\\NAS\\Media\\".into(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        share.identity,
        DeviceIdentity::Share {
            unc: "\\\\nas\\media".into()
        }
    );
    assert_eq!(share.display_name, "media");
    let bad = c
        .request(Request::AddNetworkShare { unc: "nas".into() })
        .await
        .unwrap();
    assert!(matches!(bad, Response::Error { .. }));

    let mut ix = DeviceIndex::from_raw(
        0,
        vec![RawEntry {
            id: 1,
            parent_id: 0,
            name: "movie.mkv".into(),
            size: 5,
            modified: 0,
            created: 0,
            attrs: 0,
        }],
    );
    let hdr = IndexHeader {
        format_version: 1,
        device_id: share.id,
        host_id: uuid::Uuid::nil(),
        checkpoint: None,
        saved_at: 0,
        rows: 0,
        sync_cursor: None,
    };
    let data_b64 =
        base64::engine::general_purpose::STANDARD.encode(encode_index(&hdr, &mut ix).unwrap());
    let r = c
        .request(Request::PushShareIndex {
            device_id: share.id,
            data_b64,
        })
        .await
        .unwrap();
    assert_eq!(r, Response::Ok);
    let (sid, n) = total(search(&mut c, "movie").await);
    assert_eq!(n, 1);
    let Response::Rows { rows, .. } = c
        .request(Request::FetchRows {
            search_id: sid,
            offset: 0,
            limit: 5,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(rows[0].full_path, "\\\\nas\\media\\movie.mkv");

    // Replacing the share's index renumbers its rows: the old result set must refuse to serve them
    // rather than return whatever file now sits at the same row number.
    let data_b64 =
        base64::engine::general_purpose::STANDARD.encode(encode_index(&hdr, &mut ix).unwrap());
    assert_eq!(
        c.request(Request::PushShareIndex {
            device_id: share.id,
            data_b64
        })
        .await
        .unwrap(),
        Response::Ok
    );
    let stale = c
        .request(Request::FetchRows {
            search_id: sid,
            offset: 0,
            limit: 5,
        })
        .await
        .unwrap();
    assert!(matches!(stale, Response::Error { .. }), "got {stale:?}");
    let (sid2, _) = total(search(&mut c, "movie").await);
    assert!(sid2 > sid, "search ids keep increasing");

    // Unplug, then forget the stick: its index file is removed.
    fake.vols.lock().unwrap().clear();
    wait_for(&mut c, "offline", |ds| {
        ds.iter()
            .any(|d| d.id == id && d.state == DeviceState::Offline)
    })
    .await;
    assert_eq!(
        c.request(Request::ForgetDevice { id }).await.unwrap(),
        Response::Ok
    );
    assert!(!dir
        .path()
        .join("indexes")
        .join(format!("{id}.seekidx"))
        .exists());
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
        cb(Changed::Everything);
        cb(Changed::Dirs(
            [std::path::PathBuf::from(r"T:\folder1")]
                .into_iter()
                .collect(),
        ));
        cb(Changed::Everything);
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        *fake.scans.lock().unwrap(),
        1,
        "rescan must wait for the cooldown"
    );
    for _ in 0..200 {
        if *fake.scans.lock().unwrap() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(*fake.scans.lock().unwrap(), 2);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        *fake.scans.lock().unwrap(),
        2,
        "no further scans without new changes"
    );

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
                        .request(Request::Search {
                            query: q,
                            sort: SortKey::Name,
                            descending: false,
                        })
                        .await
                        .unwrap();
                    // a newer search on the same connection may cancel an older one
                    assert!(
                        matches!(r, Response::SearchStarted { .. } | Response::Error { .. }),
                        "{r:?}"
                    );
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

#[tokio::test(flavor = "multi_thread")]
async fn session_only_volume_is_fed_by_the_app() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.vols.lock().unwrap().push(volume()); // T: is visible to the service itself
    let svc = start(dir.path(), fake.clone()).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "device scanned", scanned_online).await;

    // The app reports T: too: the service already indexes it, so the app must leave it alone.
    let t = UserVolume {
        mount_point: "T:\\".into(),
        identity: volume().identity,
        label: "TESTSTICK".into(),
        filesystem: "exFAT".into(),
        kind: DeviceKind::Removable,
        total_bytes: 0,
        free_bytes: 0,
    };
    assert_eq!(
        c.request(Request::AddUserVolume { volume: t })
            .await
            .unwrap(),
        Response::Ok
    );

    // G: only exists in the user's session (Google Drive): the app scans it and pushes the index.
    let g = UserVolume {
        mount_point: "G:\\".into(),
        identity: DeviceIdentity::Volume {
            serial: 0x1234,
            filesystem: "FAT32".into(),
        },
        label: "Google Drive".into(),
        filesystem: "FAT32".into(),
        kind: DeviceKind::Fixed,
        total_bytes: 1 << 40,
        free_bytes: 1 << 39,
    };
    let Response::Device { device } = c
        .request(Request::AddUserVolume { volume: g.clone() })
        .await
        .unwrap()
    else {
        panic!("expected a device for a volume the service cannot see")
    };
    assert_eq!(device.display_name, "Google Drive (G:)");
    // reporting it again keeps the same device
    let Response::Device { device: again } = c
        .request(Request::AddUserVolume { volume: g })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(again.id, device.id);

    let mut ix = DeviceIndex::from_raw(
        0,
        vec![RawEntry {
            id: 1,
            parent_id: 0,
            name: "holiday-plan.gdoc".into(),
            size: 1,
            modified: 0,
            created: 0,
            attrs: 0,
        }],
    );
    let hdr = IndexHeader {
        format_version: 1,
        device_id: device.id,
        host_id: uuid::Uuid::nil(),
        checkpoint: None,
        saved_at: 0,
        rows: 0,
        sync_cursor: None,
    };
    let data_b64 =
        base64::engine::general_purpose::STANDARD.encode(encode_index(&hdr, &mut ix).unwrap());
    assert_eq!(
        c.request(Request::PushShareIndex {
            device_id: device.id,
            data_b64
        })
        .await
        .unwrap(),
        Response::Ok
    );
    let (sid, n) = total(search(&mut c, "holiday-plan").await);
    assert_eq!(n, 1);
    let Response::Rows { rows, .. } = c
        .request(Request::FetchRows {
            search_id: sid,
            offset: 0,
            limit: 5,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(rows[0].full_path, r"G:\holiday-plan.gdoc");
    assert!(rows[0].online);

    // Drive quits: offline, still searchable.
    assert_eq!(
        c.request(Request::SetShareState {
            device_id: device.id,
            online: false
        })
        .await
        .unwrap(),
        Response::Ok
    );
    assert_eq!(total(search(&mut c, "offline: holiday-plan").await).1, 1);

    // The service's own volumes cannot be overwritten by the app.
    let Response::Devices { devices } = c.request(Request::ListDevices).await.unwrap() else {
        panic!()
    };
    let own = devices
        .iter()
        .find(|d| d.display_name.contains("TESTSTICK"))
        .unwrap()
        .id;
    let r = c
        .request(Request::SetShareState {
            device_id: own,
            online: false,
        })
        .await
        .unwrap();
    assert!(matches!(r, Response::Error { .. }), "{r:?}");

    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_folders_are_rescanned_without_a_full_scan() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.vols.lock().unwrap().push(volume());
    let svc = start(dir.path(), fake.clone()).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "device scanned", scanned_online).await;
    assert_eq!(total(search(&mut c, "fresh").await).1, 0);

    *fake.folder_changes.lock().unwrap() = vec![Change::Upsert(RawEntry {
        id: 5000,
        parent_id: 1,
        name: "fresh.txt".into(),
        size: 7,
        modified: 1_700_000_000_000,
        created: 1_700_000_000_000,
        attrs: 0,
    })];
    let folder = std::path::PathBuf::from(r"T:\folder1");
    {
        let cb = fake.on_change.lock().unwrap();
        let cb = cb.as_ref().expect("a watcher was attached after the scan");
        cb(Changed::Dirs([folder.clone()].into_iter().collect()));
        cb(Changed::Dirs([folder.clone()].into_iter().collect()));
    }
    // No cooldown for folder updates: visible within a couple of monitor ticks.
    let mut found = 0;
    for _ in 0..100 {
        found = total(search(&mut c, "fresh").await).1;
        if found == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(found, 1);
    assert_eq!(*fake.scans.lock().unwrap(), 1, "no full rescan");
    assert_eq!(
        *fake.rescanned.lock().unwrap(),
        vec![folder],
        "both reports merged into one update"
    );
    wait_for(&mut c, "entry count updated", |ds| {
        ds[0].entry_count == ENTRIES + 1
    })
    .await;

    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn other_users_folders_are_hidden_unless_elevated() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.vols.lock().unwrap().push(volume());
    *fake.profiles.lock().unwrap() = vec![
        (ME.into(), r"T:\folder1".into()),
        ("S-1-5-21-bob".into(), r"T:\FOLDER2\".into()),
        ("S-1-5-21-eve".into(), r"Z:\elsewhere".into()),
    ];
    let svc = start(dir.path(), fake.clone()).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "device scanned", scanned_online).await;

    // folder2 holds files 11, 21, …, 991 (99 files); my own folder1 stays visible.
    assert_eq!(total(search(&mut c, "file").await).1, 990 - 99);
    assert_eq!(
        total(search(&mut c, "folder").await).1,
        9,
        "the folder itself is hidden too"
    );
    assert_eq!(total(search(&mut c, "file11.txt").await).1, 0);
    assert_eq!(
        total(search(&mut c, "file20.txt").await).1,
        1,
        "folder1 is mine"
    );

    *fake.elevated.lock().unwrap() = true;
    let mut admin = connect(&svc.pipe).await;
    assert_eq!(total(search(&mut admin, "file").await).1, 990);
    assert_eq!(total(search(&mut admin, "folder").await).1, 10);

    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn usb_stick_known_from_another_pc_gets_its_own_local_record() {
    let dir = tempfile::tempdir().unwrap();
    // devices.json already holds a copy of the stick indexed by a linked PC
    let other_pc = uuid::Uuid::new_v4();
    let remote = seek_core::Device {
        id: uuid::Uuid::new_v4(),
        identity: volume().identity,
        display_name: "TESTSTICK (E:)".into(),
        label: "TESTSTICK".into(),
        filesystem: "exFAT".into(),
        kind: DeviceKind::Removable,
        total_bytes: 0,
        free_bytes: 0,
        last_mount_point: "E:\\".into(),
        indexed_by_host: other_pc,
        first_seen: 1,
        last_seen: 1,
        index_updated_at: 1,
        state: DeviceState::Offline,
        entry_count: 7,
        error: None,
    };
    let reg = seek_core::DeviceRegistry {
        host_id: uuid::Uuid::new_v4(),
        devices: vec![remote.clone()],
    };
    reg.save(dir.path()).unwrap();

    let fake = Arc::new(Fake::default());
    fake.vols.lock().unwrap().push(volume());
    let svc = start(dir.path(), fake.clone()).await;
    let mut c = connect(&svc.pipe).await;
    let host = reg.host_id;
    wait_for(&mut c, "a local record scanned", |ds| {
        ds.iter().any(|d| {
            d.indexed_by_host == host && d.entry_count == ENTRIES && d.state == DeviceState::Online
        })
    })
    .await;
    let Response::Devices { devices } = c.request(Request::ListDevices).await.unwrap() else {
        panic!()
    };
    // the connected local record wins; the other PC's copy of the same stick is hidden, not lost
    assert_eq!(
        devices.iter().filter(|d| d.identity == volume().identity).count(),
        1
    );
    assert!(devices.iter().all(|d| d.id != remote.id));
    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
    let saved = seek_core::DeviceRegistry::load_or_create(dir.path()).unwrap();
    let copy = saved
        .devices
        .iter()
        .find(|d| d.id == remote.id)
        .expect("the other PC's record is kept");
    assert_eq!(
        (copy.entry_count, copy.indexed_by_host),
        (7, other_pc),
        "never scanned locally"
    );
    assert_eq!(*fake.scans.lock().unwrap(), 1);
}

async fn device_id(c: &mut Client) -> seek_core::DeviceId {
    match c.request(Request::ListDevices).await.unwrap() {
        Response::Devices { devices } => devices[0].id,
        other => panic!("expected Devices, got {other:?}"),
    }
}

async fn space_tree(c: &mut Client, device_id: seek_core::DeviceId, path: &str, depth: u8) -> Response {
    c.request(Request::SpaceTree { device_id, path: path.into(), depth }).await.unwrap()
}

fn node(r: Response) -> seek_core::SpaceNode {
    match r {
        Response::SpaceTree { node } => node,
        other => panic!("expected SpaceTree, got {other:?}"),
    }
}

// Files 11..=1000 live in folder(id % 10 + 1); folder1 holds 20, 30, …, 1000.
const ALL_BYTES: u64 = (11 + 1000) * 990 / 2;

#[tokio::test(flavor = "multi_thread")]
async fn space_tree_sums_the_drive() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.vols.lock().unwrap().push(volume());
    let svc = start(dir.path(), fake.clone()).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "device scanned", scanned_online).await;
    let id = device_id(&mut c).await;

    let root = node(space_tree(&mut c, id, "T:\\", 2).await);
    assert_eq!((root.bytes, root.files), (ALL_BYTES, 990));
    let names: Vec<&str> = root.children.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names.len(), 10);
    assert_eq!(&names[..3], ["folder1", "folder10", "folder9"]);
    assert_eq!(root.children[0].bytes, 10 * (2..=100).sum::<u64>());
    assert!(root.children[0].children.iter().all(|f| f.kind == seek_core::SpaceKind::File || f.files > 1));
    assert_eq!(node(space_tree(&mut c, id, "t:", 1).await).bytes, ALL_BYTES, "mount without backslash, any case");

    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn space_tree_of_a_folder_and_of_a_missing_one() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.vols.lock().unwrap().push(volume());
    let svc = start(dir.path(), fake.clone()).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "device scanned", scanned_online).await;
    let id = device_id(&mut c).await;

    let f3 = node(space_tree(&mut c, id, r"T:\FOLDER3\", 1).await);
    assert_eq!((f3.name.as_str(), f3.files), ("folder3", 99));
    assert_eq!(f3.children.len(), 99);
    match space_tree(&mut c, id, r"T:\nope", 1).await {
        Response::Error { message } => assert_eq!(message, "That folder no longer exists"),
        other => panic!("expected Error, got {other:?}"),
    }
    match space_tree(&mut c, uuid::Uuid::new_v4(), "T:\\", 1).await {
        Response::Error { message } => assert_eq!(message, "Unknown drive"),
        other => panic!("expected Error, got {other:?}"),
    }

    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn space_largest_files_and_types() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.vols.lock().unwrap().push(volume());
    let svc = start(dir.path(), fake.clone()).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "device scanned", scanned_online).await;
    let id = device_id(&mut c).await;

    let files = match c.request(Request::SpaceLargest { device_id: id, path: "T:\\".into(), limit: 5 }).await.unwrap() {
        Response::SpaceFiles { files } => files,
        other => panic!("expected SpaceFiles, got {other:?}"),
    };
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["file1000.txt", "file999.txt", "file998.txt", "file997.txt", "file996.txt"]);
    assert_eq!(files[0].full_path, r"T:\folder1\file1000.txt");
    let types = match c.request(Request::SpaceTypes { device_id: id, path: r"T:\folder1".into() }).await.unwrap() {
        Response::SpaceTypes { types } => types,
        other => panic!("expected SpaceTypes, got {other:?}"),
    };
    assert_eq!(
        types,
        [seek_core::TypeTotal { category: seek_core::TypeCategory::Documents, bytes: 10 * (2..=100).sum::<u64>(), files: 99 }]
    );

    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn space_view_hides_other_users_folders() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.vols.lock().unwrap().push(volume());
    *fake.profiles.lock().unwrap() = vec![(ME.into(), r"T:\folder1".into()), ("S-1-5-21-bob".into(), r"T:\FOLDER2\".into())];
    let svc = start(dir.path(), fake.clone()).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "device scanned", scanned_online).await;
    let id = device_id(&mut c).await;

    let bob_bytes = (11 + 991) * 99 / 2;
    let root = node(space_tree(&mut c, id, "T:\\", 3).await);
    assert_eq!(root.bytes, ALL_BYTES, "totals stay true");
    assert!(!root.children.iter().any(|n| n.name == "folder2"));
    let other = root.children.iter().find(|n| n.kind == seek_core::SpaceKind::Hidden).unwrap();
    assert_eq!((other.name.as_str(), other.bytes, other.files), ("Other users", bob_bytes, 99));
    assert!(matches!(space_tree(&mut c, id, r"T:\folder2", 1).await, Response::Error { .. }));
    match c.request(Request::SpaceLargest { device_id: id, path: "T:\\".into(), limit: 1000 }).await.unwrap() {
        Response::SpaceFiles { files } => {
            assert_eq!(files.len(), 990 - 99);
            assert!(files.iter().all(|f| !f.folder.ends_with("folder2")));
        }
        other => panic!("expected SpaceFiles, got {other:?}"),
    }

    *fake.elevated.lock().unwrap() = true;
    let mut admin = connect(&svc.pipe).await;
    let root = node(space_tree(&mut admin, id, "T:\\", 1).await);
    assert!(root.children.iter().any(|n| n.name == "folder2"));
    assert!(!root.children.iter().any(|n| n.kind == seek_core::SpaceKind::Hidden));

    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
}

async fn same_size(
    c: &mut Client,
    device_ids: &[seek_core::DeviceId],
    min_size: u64,
    cursor: Option<u64>,
    limit: u32,
) -> (Vec<seek_ipc::SizeGroup>, Option<u64>) {
    match c
        .request(Request::SameSizeFiles { min_size, device_ids: device_ids.to_vec(), cursor, limit })
        .await
        .unwrap()
    {
        Response::SameSizeFiles { groups, next } => (groups, next),
        other => panic!("expected SameSizeFiles, got {other:?}"),
    }
}

/// Every page of a query, following `next`; also checks each page against the paging rules.
async fn same_size_all(
    c: &mut Client,
    ids: &[seek_core::DeviceId],
    min_size: u64,
    limit: u32,
) -> Vec<seek_ipc::SizeGroup> {
    let (mut all, mut cursor) = (Vec::new(), None);
    for _ in 0..2000 {
        let (groups, next) = same_size(c, ids, min_size, cursor, limit).await;
        assert!(!groups.is_empty(), "a page always holds a group");
        let files: usize = groups.iter().map(|g| g.files.len()).sum();
        assert!(files <= limit.max(1) as usize || groups.len() == 1, "{files} files > limit {limit}");
        if let (Some(n), Some(last)) = (next, groups.last()) {
            assert!(n < last.size, "the cursor moves down");
        }
        all.extend(groups);
        match next {
            Some(n) => cursor = Some(n),
            None => return all,
        }
    }
    panic!("paging never finished");
}

fn file_number(row: &seek_core::ResultRow) -> u64 {
    row.name.trim_start_matches("file").trim_end_matches(".txt").parse().unwrap()
}

// With size_mod 100, file ids s, s+100, … share size s: sizes 1..=10 hold 9 files (ids 1..=10 are
// folders), sizes 11..=99 hold 10. Size 0 (ids 100, 200, …) is left out by min_size 1.
const SAME_SIZE_FILES: usize = 10 * 9 + 89 * 10;

#[tokio::test(flavor = "multi_thread")]
async fn same_size_files_pages_through_every_group() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    *fake.size_mod.lock().unwrap() = 100;
    fake.vols.lock().unwrap().push(volume());
    let svc = start(dir.path(), fake.clone()).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "device scanned", scanned_online).await;
    let id = device_id(&mut c).await;

    let all = same_size_all(&mut c, &[id], 1, 25).await;
    let sizes: Vec<u64> = all.iter().map(|g| g.size).collect();
    assert_eq!(sizes, (1..=99).rev().collect::<Vec<u64>>());
    assert_eq!(all.iter().map(|g| g.files.len()).sum::<usize>(), SAME_SIZE_FILES);
    for g in &all {
        assert_eq!(g.files.len(), if g.size <= 10 { 9 } else { 10 }, "size {}", g.size);
        assert!(g.files.iter().all(|f| f.size == g.size && !f.is_dir && f.device_id == id));
        let nums: Vec<u64> = g.files.iter().map(file_number).collect();
        assert!(nums.windows(2).all(|w| w[0] < w[1]), "rows in order: {nums:?}");
    }
    assert_eq!(all[0].files[0].full_path, r"T:\folder10\file99.txt");

    // a group alone can be bigger than the limit: one group per page then
    let one = same_size_all(&mut c, &[id], 1, 1).await;
    assert_eq!(one, all);
    let (page, next) = same_size(&mut c, &[id], 1, Some(50), 0).await;
    assert_eq!((page.len(), page[0].size, next), (1, 50, Some(49)));
    let (page, next) = same_size(&mut c, &[id], 1, Some(1), 1000).await;
    assert_eq!((page.len(), page[0].size, next), (1, 1, None));
    // the minimum is inclusive, and a page past the end is empty
    let (page, next) = same_size(&mut c, &[id], 99, None, 1000).await;
    assert_eq!((page.len(), next), (1, None));
    assert_eq!(same_size(&mut c, &[id], 100, None, 1000).await, (vec![], None));
    assert_eq!(same_size(&mut c, &[id], 1, Some(0), 10).await, (vec![], None));

    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn same_size_files_survives_an_idle_unload_between_pages() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    *fake.size_mod.lock().unwrap() = 100;
    fake.vols.lock().unwrap().push(volume());
    let svc = start_idle(dir.path(), fake.clone(), Some(Duration::from_millis(1))).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "device scanned", scanned_online).await;
    let id = device_id(&mut c).await;

    let (first, next) = same_size(&mut c, &[id], 1, None, 25).await;
    let next = next.expect("more pages");

    // wait until the index is gone (a stored search disappears with it)
    let (search_id, _) = total(search(&mut c, "file").await);
    let mut gone = false;
    for _ in 0..240 {
        if let Response::Error { .. } = c.request(Request::FetchRows { search_id, offset: 0, limit: 1 }).await.unwrap() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone, "the index was never unloaded");

    let (second, _) = same_size(&mut c, &[id], 1, Some(next), 25).await;
    assert_eq!(second[0].size, next);
    assert!(second[0].size < first.last().unwrap().size);
    let all = same_size_all(&mut c, &[id], 1, 25).await;
    assert_eq!(all.len(), 99);

    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn same_size_files_hides_other_users_rows() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    *fake.size_mod.lock().unwrap() = 100;
    fake.vols.lock().unwrap().push(volume());
    *fake.profiles.lock().unwrap() = vec![(ME.into(), r"T:\folder1".into()), ("S-1-5-21-bob".into(), r"T:\FOLDER2\".into())];
    let svc = start(dir.path(), fake.clone()).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "device scanned", scanned_online).await;
    let id = device_id(&mut c).await;

    // folder2 holds every file whose id ends in 1, so the sizes ending in 1 vanish entirely
    let all = same_size_all(&mut c, &[id], 1, 1000).await;
    assert_eq!(all.len(), 99 - 10);
    assert!(all.iter().all(|g| g.size % 10 != 1));
    assert!(all.iter().flat_map(|g| &g.files).all(|f| !f.folder.to_lowercase().ends_with("folder2")));

    *fake.elevated.lock().unwrap() = true;
    let mut admin = connect(&svc.pipe).await;
    let all = same_size_all(&mut admin, &[id], 1, 1000).await;
    assert_eq!(all.len(), 99);
    assert_eq!(all.iter().map(|g| g.files.len()).sum::<usize>(), SAME_SIZE_FILES);

    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn same_size_counts_across_two_drives() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut second = volume();
    second.mount_point = "U:\\".into();
    second.label = "OTHERSTICK".into();
    second.identity = DeviceIdentity::Volume { serial: 0xBEEF, filesystem: "exFAT".into() };
    fake.vols.lock().unwrap().extend([volume(), second]);
    let svc = start(dir.path(), fake.clone()).await;
    let mut c = connect(&svc.pipe).await;
    wait_for(&mut c, "both scanned", |ds| {
        ds.len() == 2 && ds.iter().all(|d| d.state == DeviceState::Online && d.entry_count == ENTRIES)
    })
    .await;
    let Response::Devices { devices } = c.request(Request::ListDevices).await.unwrap() else { panic!() };
    let mut ids: Vec<_> = devices.iter().map(|d| d.id).collect();
    ids.sort();

    // every size is unique inside a drive, so only the two drives together make groups
    assert_eq!(same_size(&mut c, &ids[..1], 1, None, 100).await, (vec![], None));
    assert_eq!(same_size(&mut c, &ids[1..], 1, None, 100).await, (vec![], None));
    let all = same_size_all(&mut c, &ids, 1, 100).await;
    assert_eq!(all.len(), 990);
    assert_eq!(all[0].size, 1000);
    assert_eq!(all.last().unwrap().size, 11);
    for g in &all {
        assert_eq!(g.files.len(), 2);
        assert_eq!((g.files[0].device_id, g.files[1].device_id), (ids[0], ids[1]), "ordered by device");
        assert!(g.files.iter().all(|f| f.size == g.size));
    }

    // no drives chosen means no groups; unknown ids are skipped
    assert_eq!(same_size(&mut c, &[], 1, None, 100).await, (vec![], None));
    assert_eq!(same_size(&mut c, &[uuid::Uuid::new_v4()], 1, None, 100).await, (vec![], None));
    let with_unknown = [ids[0], uuid::Uuid::new_v4(), ids[1]];
    let (page, next) = same_size(&mut c, &with_unknown, 996, None, 100).await;
    assert_eq!((page.len(), next), (5, None));
    let (page, next) = same_size(&mut c, &ids, 1, None, 10).await;
    assert_eq!((page.len(), next), (5, Some(995)));

    let _ = svc.stop.send(true);
    svc.task.await.unwrap().unwrap();
}
