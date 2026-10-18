//! End-to-end tests: the real service loop and pipe server, with a fake volume source.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use seek_core::{
    encode_index, Change, Checkpoint, DeviceIdentity, DeviceIndex, DeviceKind, DeviceState,
    IndexHeader, RawEntry, SortKey, ATTR_DIRECTORY,
};
use seek_ipc::{Client, MuxClient, Request, Response, UserVolume};
use seek_scan::{Changed, DirWatcher, FollowError, ScanError, ScanResult, UsnFollower, VolumeInfo};
use seek_service::access::Caller;
use seek_service::{Config, VolumeSource};
use tokio::net::windows::named_pipe::NamedPipeServer;

const ENTRIES: u64 = 1000;

fn entries() -> Vec<RawEntry> {
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
            index: DeviceIndex::from_raw(0, entries()),
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
    let pipe = format!(r"\\.\pipe\seek-test-{}", uuid::Uuid::new_v4());
    let (stop, rx) = tokio::sync::watch::channel(false);
    let cfg = Config {
        data_dir: dir.to_path_buf(),
        pipe_name: pipe.clone(),
        sync: None,
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
            protocol_version: 1,
        })
        .await
        .unwrap();
    assert!(matches!(
        hello,
        Response::Hello {
            protocol_version: 1,
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
