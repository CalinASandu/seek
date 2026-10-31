//! Shared by the end-to-end tests: services with fake volumes, and polling helpers.
#![allow(dead_code)] // each test binary uses its own part

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use seek_core::{
    Change, Checkpoint, DeviceIdentity, DeviceIndex, DeviceKind, RawEntry, SortKey, ATTR_DIRECTORY,
};
use seek_ipc::{Client, Request, Response};
use seek_scan::{Changed, DirWatcher, FollowError, ScanError, ScanResult, UsnFollower, VolumeInfo};
use seek_service::access::Caller;
use seek_service::peers::PeersRuntime;
use seek_service::{Config, State, SyncConfig, VolumeSource};
use seek_sync::peers::PeerState;
use seek_sync::transport::TlsConfig;
use tokio::net::windows::named_pipe::NamedPipeServer;

pub const ENTRIES: u64 = 1000;
pub const ME: &str = "S-1-5-21-me";

/// `ENTRIES` entries: folders `folder1..10`, files `<prefix><id>.txt` spread over them.
pub fn entries(prefix: &str) -> Vec<RawEntry> {
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

pub fn volume(serial: u64) -> VolumeInfo {
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

pub type OnChange = Box<dyn Fn(Changed) + Send>;

pub struct Fake {
    pub vols: Mutex<Vec<VolumeInfo>>,
    pub prefix: String,
    pub on_change: Mutex<Option<OnChange>>,
    pub folder_changes: Mutex<Vec<Change>>,
    /// The pipe client is this user (SID), elevated or not; `profiles` = (SID, folder).
    pub sid: Mutex<String>,
    pub elevated: Mutex<bool>,
    pub profiles: Mutex<Vec<(String, String)>>,
}

impl Fake {
    pub fn new(prefix: &str, vols: Vec<VolumeInfo>) -> Arc<Fake> {
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

pub struct Pc {
    pub pipe: String,
    pub state: Arc<State>,
    pub stop: tokio::sync::watch::Sender<bool>,
    pub task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Pc {
    pub fn rt(&self) -> &Arc<PeersRuntime> {
        self.state.peers_rt.get().expect("sync is enabled")
    }
    pub fn id(&self) -> uuid::Uuid {
        self.state.host_id()
    }
    pub fn addr(&self) -> String {
        self.rt().local_addr().to_string()
    }
    pub fn peer_state(&self, other: &Pc) -> Option<PeerState> {
        self.state.peers.lock().unwrap().get(other.id()).map(|p| p.state)
    }
    pub fn connected_to(&self, other: &Pc) -> bool {
        self.state.connected.lock().unwrap().contains(&other.id())
    }
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
    /// A pipe client (the caller's SID/elevation are read from the fake when it connects).
    pub async fn client(&self) -> Client {
        for _ in 0..100 {
            if let Ok(c) = Client::connect_to(&self.pipe).await {
                return c;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("service did not start");
    }
}

pub async fn devices(c: &mut Client) -> Vec<seek_core::Device> {
    match c.request(Request::ListDevices).await.unwrap() {
        Response::Devices { devices } => devices,
        other => panic!("{other:?}"),
    }
}

pub async fn wait_devices(c: &mut Client, what: &str, pred: impl Fn(&[seek_core::Device]) -> bool) {
    for _ in 0..300 {
        if pred(&devices(c).await) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// `(search id, total)` of a search.
pub async fn search(c: &mut Client, q: &str) -> (u64, u64) {
    let r = c.request(Request::Search { query: q.into(), sort: SortKey::Name, descending: false }).await.unwrap();
    match r {
        Response::SearchStarted { search_id, total, .. } => (search_id, total),
        other => panic!("search {q:?}: {other:?}"),
    }
}

/// `guest` joins `host`'s group through the pipe API (as the app does).
pub async fn link_via_pipe(host: &Pc, guest: &Pc) {
    let code = match host.client().await.request(Request::StartPairing).await.unwrap() {
        Response::PairingCode { code, .. } => code,
        other => panic!("{other:?}"),
    };
    let r = guest.client().await.request(Request::JoinPairing { addr: host.addr(), code }).await.unwrap();
    assert_eq!(r, Response::Ok);
}

pub async fn start(dir: &Path, name: &str, fake: Arc<Fake>) -> Pc {
    start_with(dir, name, fake, Duration::from_secs(1), None).await
}

/// A service syncing every `interval`; `hub_tls` is what it trusts for a `tls://` hub.
pub async fn start_with(
    dir: &Path,
    name: &str,
    fake: Arc<Fake>,
    interval: Duration,
    hub_tls: Option<TlsConfig>,
) -> Pc {
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
            interval,
            hub_tls,
        }),
    };
    let (tx, ready) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(seek_service::run_with(cfg, fake, rx, Some(tx)));
    let state = ready.await.expect("service started");
    Pc { pipe, state, stop, task }
}

pub async fn wait(what: &str, pred: impl Fn() -> bool) {
    for _ in 0..1500 {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// `guest` joins `host`'s group.
pub async fn link(host: &Pc, guest: &Pc) {
    let code = host.rt().start_pairing(ME);
    guest.rt().join(&host.addr(), &code.code, ME).await.expect("pairing succeeds");
}
