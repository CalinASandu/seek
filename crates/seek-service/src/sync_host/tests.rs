use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use seek_core::{synth_index, Change, DeviceIndex};
use seek_sync::noise::{accept, connect, Keys};
use seek_sync::{run_session, Error, SessionCtl};

use super::*;
use crate::state::Config;
use crate::test_support::{volume, NoVolumes};

const LONG: Duration = Duration::from_secs(3600);

fn load(dir: &std::path::Path) -> Arc<State> {
    let cfg = Config {
        data_dir: dir.to_path_buf(),
        pipe_name: String::new(),
        sync: None,
        idle_unload: None,
    };
    State::load(cfg, Arc::new(NoVolumes)).unwrap()
}

/// Own device with `n` synthetic entries.
fn own_device(st: &Arc<State>, n: usize) -> DeviceId {
    let id = crate::monitor::register_volume(st, &volume());
    st.replace_index(id, synth_index(n, 1));
    id
}

fn live_ids(ix: &DeviceIndex) -> Vec<u64> {
    let mut v: Vec<u64> = (0..ix.len() as u32)
        .filter(|&r| ix.is_alive(r))
        .map(|r| ix.id(r))
        .collect();
    v.sort_unstable();
    v
}

fn copy_ids(st: &State, id: DeviceId) -> Option<Vec<u64>> {
    st.ensure_loaded(&[id], "test");
    let ix = st.indexes.read().unwrap().get(&id)?.clone();
    let ix = ix.read().unwrap();
    Some(live_ids(&ix))
}

async fn wait(what: &str, pred: impl Fn() -> bool) {
    for _ in 0..500 {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Counts what a host sends, to tell full copies from deltas.
struct Counting {
    inner: ServiceSync,
    fulls: AtomicUsize,
    deltas: AtomicUsize,
}

impl Counting {
    fn new(st: &Arc<State>) -> Arc<Counting> {
        Arc::new(Counting {
            inner: ServiceSync(st.clone()),
            fulls: AtomicUsize::new(0),
            deltas: AtomicUsize::new(0),
        })
    }
}

impl SyncHost for Counting {
    fn host_id(&self) -> uuid::Uuid {
        self.inner.host_id()
    }
    fn name(&self) -> String {
        self.inner.name()
    }
    fn shared_devices(&self) -> Vec<Device> {
        self.inner.shared_devices()
    }
    fn profiles(&self) -> Vec<(String, String)> {
        self.inner.profiles()
    }
    fn outgoing(&self, device: uuid::Uuid, have: Option<Cursor>) -> Option<Outgoing> {
        let out = self.inner.outgoing(device, have);
        match &out {
            Some(Outgoing::Full { .. }) => self.fulls.fetch_add(1, Ordering::SeqCst),
            Some(Outgoing::Delta { .. }) => self.deltas.fetch_add(1, Ordering::SeqCst),
            None => 0,
        };
        out
    }
    fn held(&self, owner: uuid::Uuid) -> Vec<(uuid::Uuid, Cursor)> {
        self.inner.held(owner)
    }
    fn held_remote(&self) -> Vec<(uuid::Uuid, Cursor)> {
        self.inner.held_remote()
    }
    fn catalog(&self, hub: uuid::Uuid, pcs: Vec<CatalogPc>, devices: Vec<Device>) {
        self.inner.catalog(hub, pcs, devices)
    }
    fn remote_devices(
        &self,
        owner: uuid::Uuid,
        devices: Vec<Device>,
        profiles: Vec<(String, String)>,
    ) {
        self.inner.remote_devices(owner, devices, profiles)
    }
    fn apply_delta(
        &self,
        owner: uuid::Uuid,
        device: uuid::Uuid,
        from: Cursor,
        to: Cursor,
        changes: Vec<Change>,
    ) -> Result<(), String> {
        self.inner.apply_delta(owner, device, from, to, changes)
    }
    fn apply_full(
        &self,
        owner: uuid::Uuid,
        device: uuid::Uuid,
        cursor: Cursor,
        bytes: Vec<u8>,
    ) -> Result<(), String> {
        self.inner.apply_full(owner, device, cursor, bytes)
    }
    fn synced(&self, peer: uuid::Uuid) {
        self.inner.synced(peer)
    }
    fn member_joined(&self, member: MemberInfo) {
        self.inner.member_joined(member)
    }
    fn unlinked_by(&self, peer: uuid::Uuid) {
        self.inner.unlinked_by(peer)
    }
}

struct Pair {
    a: Arc<SessionCtl>,
    b: Arc<SessionCtl>,
    ta: tokio::task::JoinHandle<Result<(), Error>>,
    tb: tokio::task::JoinHandle<Result<(), Error>>,
}

impl Pair {
    async fn stop(self) {
        self.a.stop.notify_one();
        self.b.stop.notify_one();
        let _ = self.ta.await;
        let _ = self.tb.await;
    }
}

/// Runs a session between A (dialer) and B (listener) over an in-memory Noise channel.
async fn connect_pair(a: Arc<dyn SyncHost>, b: Arc<dyn SyncHost>, sa: &State, sb: &State) -> Pair {
    let (ida, idb) = (a.host_id(), b.host_id());
    // as the runtime does when a session starts: the other PC is connected
    sa.connected.lock().unwrap().insert(idb);
    sb.connected.lock().unwrap().insert(ida);
    let (ka, kb) = (Keys::generate(), Keys::generate());
    let (ia, ib) = tokio::io::duplex(1 << 16);
    let kb2 = kb.clone();
    let accepting = tokio::spawn(async move { accept(ib, &kb2, |_| true).await.unwrap() });
    let (ra, wa) = connect(ia, &ka, &kb.public).await.unwrap();
    let (_, rb, wb) = accepting.await.unwrap();
    let (ctl_a, out_a) = SessionCtl::new();
    let (ctl_b, out_b) = SessionCtl::new();
    let ta = tokio::spawn(run_session(a, idb, ra, wa, ctl_a.clone(), out_a, LONG));
    let tb = tokio::spawn(run_session(b, ida, rb, wb, ctl_b.clone(), out_b, LONG));
    Pair {
        a: ctl_a,
        b: ctl_b,
        ta,
        tb,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn full_copy_cursor_matches_contents() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = (load(da.path()), load(db.path()));
    let dev = own_device(&a, 1_000);

    // the index keeps changing while the first full copy is prepared
    let files: Vec<u64> = {
        let ix = a.indexes.read().unwrap()[&dev].clone();
        let ix = ix.read().unwrap();
        (0..ix.len() as u32)
            .filter(|&r| !ix.is_dir(r))
            .map(|r| ix.id(r))
            .take(200)
            .collect()
    };
    assert_eq!(files.len(), 200);
    let a2 = a.clone();
    let churn = std::thread::spawn(move || {
        for id in files {
            a2.apply_changes(dev, &[Change::Delete { id }], None);
        }
    });
    let p = connect_pair(
        Arc::new(ServiceSync(a.clone())),
        Arc::new(ServiceSync(b.clone())),
        &a,
        &b,
    )
    .await;
    churn.join().unwrap();
    p.a.sync_now.notify_one();

    let want = copy_ids(&a, dev).unwrap();
    assert_eq!(want.len(), 800);
    let cursor = a.journal_cursor(dev);
    wait("B's copy to match A", || {
        copy_ids(&b, dev).as_ref() == Some(&want)
            && b.remote_cursors.lock().unwrap().get(&dev) == Some(&cursor)
            && b.device(dev).is_some_and(|d| d.entry_count == 800)
    })
    .await;
    assert_eq!(b.device(dev).unwrap().indexed_by_host, a.host_id());
    p.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_copy_survives_restart_and_resumes_with_delta() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = (load(da.path()), load(db.path()));
    let dev = own_device(&a, 1_000);

    let ca = Counting::new(&a);
    let p = connect_pair(ca.clone(), Arc::new(ServiceSync(b.clone())), &a, &b).await;
    wait("first copy", || {
        copy_ids(&b, dev).is_some_and(|v| v.len() == 1_000)
    })
    .await;
    assert_eq!(ca.fulls.load(Ordering::SeqCst), 1);
    p.stop().await;
    b.save_dirty();
    let held = b.remote_cursors.lock().unwrap().get(&dev).copied().unwrap();
    drop(b);

    // B restarts: copy, record and cursor come back from disk
    let b = load(db.path());
    assert_eq!(b.remote_cursors.lock().unwrap().get(&dev), Some(&held));
    assert_eq!(copy_ids(&b, dev).unwrap().len(), 1_000);
    assert_eq!(b.device(dev).unwrap().indexed_by_host, a.host_id());

    a.apply_changes(dev, &[Change::Delete { id: 105 }], None);
    let ca = Counting::new(&a);
    let p = connect_pair(ca.clone(), Arc::new(ServiceSync(b.clone())), &a, &b).await;
    wait("delta applied", || {
        copy_ids(&b, dev).is_some_and(|v| v.len() == 999)
    })
    .await;
    assert_eq!(
        ca.fulls.load(Ordering::SeqCst),
        0,
        "a restart must not cost a full copy"
    );
    assert!(ca.deltas.load(Ordering::SeqCst) >= 1);
    p.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unsharing_removes_the_copy() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = (load(da.path()), load(db.path()));
    let dev = own_device(&a, 500);
    let p = connect_pair(
        Arc::new(ServiceSync(a.clone())),
        Arc::new(ServiceSync(b.clone())),
        &a,
        &b,
    )
    .await;
    wait("first copy", || copy_ids(&b, dev).is_some()).await;
    assert!(
        b.index_path(dev).exists(),
        "a full copy is saved right away"
    );

    a.peers.lock().unwrap().file.unshared.push(dev);
    p.a.sync_now.notify_one();
    wait("copy removed", || {
        b.device(dev).is_none() && copy_ids(&b, dev).is_none()
    })
    .await;
    assert!(!b.index_path(dev).exists());
    assert!(b.remote_cursors.lock().unwrap().get(&dev).is_none());
    p.stop().await;
}

fn record(id: DeviceId, host: HostId) -> Device {
    use seek_core::{DeviceIdentity, DeviceKind};
    Device {
        id,
        identity: DeviceIdentity::Volume { serial: id.as_u128() as u64, filesystem: "NTFS".into() },
        display_name: "Data".into(),
        label: String::new(),
        filesystem: "NTFS".into(),
        kind: DeviceKind::Fixed,
        total_bytes: 0,
        free_bytes: 0,
        last_mount_point: "D:\\".into(),
        indexed_by_host: host,
        first_seen: 0,
        last_seen: 0,
        index_updated_at: 0,
        state: DeviceState::Online,
        entry_count: 0,
        error: None,
    }
}

fn catalog_pc(host: HostId, name: &str, sid: &str) -> CatalogPc {
    CatalogPc {
        host_id: host,
        name: name.into(),
        user_sid: sid.into(),
        user_name: String::new(),
        profiles: vec![(sid.into(), format!("C:\\Users\\{name}"))],
        last_seen_ms: 7,
    }
}

fn peer(host: HostId, role: seek_sync::Role) -> Peer {
    Peer {
        host_id: host,
        name: "peer".into(),
        public_key: String::new(),
        addr: None,
        linked_users: Vec::new(),
        profiles: Vec::new(),
        last_sync_ms: 0,
        state: PeerState::Linked,
        role,
        addrs: Vec::new(),
        via_hub: false,
    }
}

/// A hub peer linked by the local user `S-me`.
fn hub_peer(hub: HostId) -> Peer {
    let mut p = peer(hub, seek_sync::Role::Hub);
    p.linked_users = vec![("S-me".into(), "S-me".into())];
    p
}

#[test]
fn catalog_adds_and_drops_pcs() {
    let dir = tempfile::tempdir().unwrap();
    let st = load(dir.path());
    let (hub, b, c) = (HostId::from_u128(1), HostId::from_u128(2), HostId::from_u128(3));
    let (db, dc) = (DeviceId::from_u128(20), DeviceId::from_u128(30));
    st.peers.lock().unwrap().upsert(hub_peer(hub));
    let host = ServiceSync(st.clone());

    host.catalog(
        hub,
        vec![catalog_pc(b, "B", "S-b"), catalog_pc(c, "C", "S-c")],
        vec![record(db, b), record(dc, c)],
    );
    {
        let peers = st.peers.lock().unwrap();
        let pb = peers.get(b).expect("B is stored");
        assert!(pb.via_hub && pb.role == seek_sync::Role::Pc && pb.state == PeerState::Linked && pb.addrs.is_empty());
        assert_eq!(pb.name, "B");
        // one person on every PC of the hub: (local linking SID, that PC's SID)
        assert_eq!(pb.linked_users, vec![("S-me".to_string(), "S-b".to_string())]);
        assert_eq!(pb.profiles, vec![("S-b".to_string(), "C:\\Users\\B".to_string())]);
        assert!(peers.get(c).is_some());
        assert!(peers.get(hub).is_some(), "the hub itself stays");
    }
    assert_eq!(st.device(db).unwrap().indexed_by_host, b);
    assert!(st.device(dc).is_some());
    // the linking user sees B's own profile folder, so it is not hidden from them
    let dev = st.device(db).unwrap();
    assert!(st.hidden_for(&dev, "S-me", false, &[]).is_empty());
    assert_eq!(st.hidden_for(&dev, "S-other", false, &[]), vec!["C:\\Users\\B".to_string()]);

    // C left the hub: its record and copies go, B stays
    host.catalog(hub, vec![catalog_pc(b, "B", "S-b")], vec![record(db, b)]);
    let peers = st.peers.lock().unwrap();
    assert!(peers.get(b).is_some() && peers.get(c).is_none());
    drop(peers);
    assert!(st.device(db).is_some() && st.device(dc).is_none());
    // and it is persisted
    assert!(seek_sync::peers::PeerStore::load(dir.path()).get(c).is_none());
}

#[test]
fn linking_hub_removes_direct_peers_keeps_copies() {
    let dir = tempfile::tempdir().unwrap();
    let st = load(dir.path());
    let (lan, hub) = (HostId::from_u128(5), HostId::from_u128(1));
    let dev = DeviceId::from_u128(50);
    let mut direct = peer(lan, seek_sync::Role::Pc);
    direct.addr = Some("10.0.0.2:47321".into());
    st.peers.lock().unwrap().upsert(direct);
    st.registry.write().unwrap().add(record(dev, lan));
    st.indexes.write().unwrap().insert(dev, Arc::new(RwLock::new(synth_index(10, 1))));

    st.enter_hub_mode(hub_peer(hub));

    {
        let peers = st.peers.lock().unwrap();
        assert_eq!(peers.file.peers.len(), 1);
        assert_eq!(peers.file.hub().map(|p| p.host_id), Some(hub));
    }
    assert!(st.device(dev).is_some() && st.indexes.read().unwrap().contains_key(&dev), "copies are kept");
    assert!(seek_sync::peers::PeerStore::load(dir.path()).get(lan).is_none());

    // the same PC shows up in the hub's catalog: its copy is adopted, not fetched again
    ServiceSync(st.clone()).catalog(hub, vec![catalog_pc(lan, "Laptop", "S-l")], vec![record(dev, lan)]);
    assert!(st.peers.lock().unwrap().get(lan).is_some_and(|p| p.via_hub));
    assert!(st.indexes.read().unwrap().contains_key(&dev));

    // unlinking from the hub drops the hub, its PCs and every copy received through it
    assert_eq!(st.leave_hub().map(|p| p.host_id), Some(hub));
    assert!(st.peers.lock().unwrap().file.peers.is_empty());
    assert!(st.device(dev).is_none() && !st.indexes.read().unwrap().contains_key(&dev));
}

#[test]
fn catalog_keeps_unlisted_ex_lan_copies_until_listed() {
    let dir = tempfile::tempdir().unwrap();
    let st = load(dir.path());
    let (hub, lan, other) = (HostId::from_u128(1), HostId::from_u128(5), HostId::from_u128(6));
    let (dev, dother) = (DeviceId::from_u128(50), DeviceId::from_u128(60));
    st.peers.lock().unwrap().upsert(peer(lan, seek_sync::Role::Pc));
    st.registry.write().unwrap().add(record(dev, lan));
    st.indexes.write().unwrap().insert(dev, Arc::new(RwLock::new(synth_index(10, 1))));
    let cursor = Cursor { generation: uuid::Uuid::from_u128(9), seq: 4 };
    st.remote_cursors.lock().unwrap().insert(dev, cursor);
    st.enter_hub_mode(hub_peer(hub));
    let host = ServiceSync(st.clone());

    // the hub knows only another PC so far: the ex-LAN copy, index and cursor are untouched
    host.catalog(hub, vec![catalog_pc(other, "Other", "S-o")], vec![record(dother, other)]);
    assert!(st.device(dev).is_some() && st.indexes.read().unwrap().contains_key(&dev));
    assert_eq!(st.remote_cursors.lock().unwrap().get(&dev), Some(&cursor));
    assert!(st.peers.lock().unwrap().get(lan).is_none());

    // that PC links to the hub later: it becomes a hub PC and keeps its cursor, so it gets deltas
    host.catalog(
        hub,
        vec![catalog_pc(other, "Other", "S-o"), catalog_pc(lan, "Laptop", "S-l")],
        vec![record(dother, other), record(dev, lan)],
    );
    assert!(st.peers.lock().unwrap().get(lan).is_some_and(|p| p.via_hub));
    assert!(st.indexes.read().unwrap().contains_key(&dev));
    assert_eq!(st.remote_cursors.lock().unwrap().get(&dev), Some(&cursor));
    assert_eq!(host.held_remote(), vec![(dev, cursor)], "the next pull reports it, so the hub sends a delta");
}

#[test]
fn identical_catalogs_announce_nothing() {
    use std::sync::atomic::Ordering;

    let dir = tempfile::tempdir().unwrap();
    let st = load(dir.path());
    let (hub, b) = (HostId::from_u128(1), HostId::from_u128(2));
    st.peers.lock().unwrap().upsert(hub_peer(hub));
    let host = ServiceSync(st.clone());
    let (pcs, devs) = (vec![catalog_pc(b, "B", "S-b")], vec![record(DeviceId::from_u128(20), b)]);

    let mut events = st.events.subscribe();
    host.catalog(hub, pcs.clone(), devs.clone());
    assert!(events.try_recv().is_ok(), "the first catalog changes things");
    while events.try_recv().is_ok() {}
    st.invalidated.store(false, Ordering::Relaxed);

    // the hub sends the same catalog every poll
    host.catalog(hub, pcs.clone(), devs.clone());
    assert!(events.try_recv().is_err(), "no DevicesChanged / PeersChanged for an unchanged catalog");
    assert!(!st.invalidated.load(Ordering::Relaxed), "results are not invalidated");

    // a real change is announced again
    let mut renamed = devs.clone();
    renamed[0].display_name = "Data 2".into();
    host.catalog(hub, pcs, renamed);
    assert!(events.try_recv().is_ok());
    assert!(st.invalidated.load(Ordering::Relaxed));
}

/// A drive that stays plugged in has a `last_seen` from when it came online, days ago. It was really last seen when
/// its PC last talked to the hub, as long as that PC reported it online.
#[test]
fn a_drive_online_on_its_pc_was_seen_when_that_pc_last_synced() {
    let dir = tempfile::tempdir().unwrap();
    let st = load(dir.path());
    let (hub, b) = (HostId::from_u128(1), HostId::from_u128(2));
    let (plugged, unplugged) = (DeviceId::from_u128(20), DeviceId::from_u128(30));
    st.peers.lock().unwrap().upsert(hub_peer(hub));
    let host = ServiceSync(st.clone());

    let pc = CatalogPc { last_seen_ms: 5_000, ..catalog_pc(b, "B", "S-b") };
    let mut on = record(plugged, b);
    on.last_seen = 1_000;
    let mut off = record(unplugged, b);
    off.state = DeviceState::Offline;
    off.last_seen = 2_000;
    host.catalog(hub, vec![pc], vec![on, off]);

    assert_eq!(st.device(plugged).unwrap().last_seen, 5_000);
    assert_eq!(st.device(unplugged).unwrap().last_seen, 2_000, "unplugged there: its PC knows when");
}

/// The other PC's last contact moves every minute. That alone must not make the app reload or searches restart.
#[test]
fn the_seen_date_moving_announces_nothing() {
    use std::sync::atomic::Ordering;

    let dir = tempfile::tempdir().unwrap();
    let st = load(dir.path());
    let (hub, b) = (HostId::from_u128(1), HostId::from_u128(2));
    let id = DeviceId::from_u128(20);
    st.peers.lock().unwrap().upsert(hub_peer(hub));
    let host = ServiceSync(st.clone());
    let at = |ms| CatalogPc { last_seen_ms: ms, ..catalog_pc(b, "B", "S-b") };

    let mut events = st.events.subscribe();
    host.catalog(hub, vec![at(5_000)], vec![record(id, b)]);
    while events.try_recv().is_ok() {}
    st.invalidated.store(false, Ordering::Relaxed);

    host.catalog(hub, vec![at(6_000)], vec![record(id, b)]);
    assert_eq!(st.device(id).unwrap().last_seen, 6_000);
    assert!(!st.invalidated.load(Ordering::Relaxed), "results are not invalidated");
    while let Ok(e) = events.try_recv() {
        assert!(!matches!(e, seek_ipc::Event::DevicesChanged), "the device list did not change");
    }
}

/// Linked directly, a PC's drives were last seen when the session with it ended.
#[test]
fn a_direct_session_ending_stamps_the_drives_it_had_online() {
    let dir = tempfile::tempdir().unwrap();
    let st = load(dir.path());
    let b = HostId::from_u128(2);
    let (on, off) = (DeviceId::from_u128(20), DeviceId::from_u128(30));
    {
        let mut reg = st.registry.write().unwrap();
        let mut a = record(on, b);
        a.last_seen = 1_000;
        reg.add(a);
        let mut c = record(off, b);
        c.state = DeviceState::Offline;
        c.last_seen = 2_000;
        reg.add(c);
    }
    let before = crate::state::now_ms();
    st.set_peer_devices_online(b, false);

    let seen = st.device(on).unwrap();
    assert_eq!(seen.state, DeviceState::Offline);
    assert!(seen.last_seen >= before, "{}", seen.last_seen);
    assert_eq!(st.device(off).unwrap().last_seen, 2_000);
}
