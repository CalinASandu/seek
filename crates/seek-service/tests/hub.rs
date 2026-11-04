//! Two real services syncing through a real in-process hub, over plain TCP and over TLS.

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use common::*;
use seek_core::{Change, Device, DeviceId, RawEntry};
use seek_hub::net::Hub;
use seek_hub::{HubConfig, HubKey, Served, Store};
use seek_ipc::{Client, Request, Response};
use seek_scan::Changed;
use seek_service::state::SharedIndex;
use seek_sync::noise::{self, Keys};
use seek_sync::pairing::{pair_guest, PairInfo};
use seek_sync::peers::{key_from_b64, key_to_b64};
use seek_sync::transport::TlsConfig;
use seek_sync::{Cursor, Frame, Msg, Refusal, Role, SYNC_PROTOCOL};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// How often the PCs poll the hub (the real default is a minute).
const FAST: Duration = Duration::from_millis(200);
const HUB_NAME: &str = "Test Hub";
/// Files a service names `alpha<id>.txt` / `bravo<id>.txt` (all but the ten folders).
const FILES: u64 = ENTRIES - 10;

struct TestHub {
    _dir: tempfile::TempDir,
    cfg: HubConfig,
    store: Arc<Store>,
    hub: Arc<Hub>,
}

impl TestHub {
    async fn start() -> TestHub {
        let dir = tempfile::tempdir().unwrap();
        let cfg = HubConfig {
            name: HUB_NAME.into(),
            listen: vec!["127.0.0.1:0".parse().unwrap()],
            public: None,
            data_dir: dir.path().join("data"),
            keys_dir: dir.path().join("keys"),
            pc_quota_bytes: 2048 * 1024 * 1024,
            pc_max_devices: 64,
        };
        let key = HubKey::create(&cfg.keys_dir).unwrap();
        let store = Arc::new(Store::open(&cfg, key).unwrap());
        let hub = Hub::start(cfg.clone(), store.clone()).await.unwrap();
        // later restarts listen on the same port, which is what the PCs have stored
        let mut cfg = cfg;
        cfg.listen = hub.local_addrs();
        TestHub { _dir: dir, cfg, store, hub }
    }

    fn sock(&self) -> SocketAddr {
        self.hub.local_addrs()[0]
    }

    /// What a PC is told to dial.
    fn addr(&self) -> String {
        self.sock().to_string()
    }

    /// Stops the hub (its sessions end) and waits until nothing listens any more.
    async fn stop(&self) {
        self.hub.shutdown();
        wait("the hub stopped listening", || std::net::TcpStream::connect(self.sock()).is_err()).await;
    }

    /// A new hub process on the same directories, key and port.
    async fn restart(&mut self) {
        self.stop().await;
        let key = HubKey::load(&self.cfg.keys_dir).unwrap();
        self.store = Arc::new(Store::open(&self.cfg, key).unwrap());
        for _ in 0..100 {
            match Hub::start(self.cfg.clone(), self.store.clone()).await {
                Ok(hub) => {
                    self.hub = hub;
                    return;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        panic!("the hub could not listen again");
    }

    fn code(&self) -> String {
        self.hub.open_pairing(chrono::Utc::now().timestamp_millis()).code
    }

    /// `pc` joins the hub as user `ME`, dialing `addr`.
    async fn link_at(&self, pc: &Pc, addr: &str) {
        let name = pc.rt().link_hub(addr, &self.code(), ME).await.expect("linking to the hub");
        assert_eq!(name, HUB_NAME);
    }

    async fn link(&self, pc: &Pc) {
        self.link_at(pc, &self.addr()).await;
    }

    /// The modified times and sizes of every stored copy: they change when anything is uploaded.
    fn copies(&self) -> Vec<(PathBuf, SystemTime, u64)> {
        let mut v: Vec<_> = seek_hub::store::files(&self.cfg.data_dir)
            .unwrap()
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "full" || e == "journal"))
            .map(|p| {
                let m = std::fs::metadata(&p).unwrap();
                (p, m.modified().unwrap(), m.len())
            })
            .collect();
        v.sort();
        v
    }
}

async fn start_pc(dir: &Path, name: &str, fake: Arc<Fake>, tls: Option<TlsConfig>) -> Pc {
    start_with(dir, name, fake, FAST, tls).await
}

/// The device `pc` indexes itself, once it has scanned it.
async fn own_device(pc: &Pc) -> DeviceId {
    let mut c = pc.client().await;
    let id = pc.id();
    wait_devices(&mut c, "a scanned drive", |ds| ds.iter().any(|d| d.indexed_by_host == id && d.entry_count == ENTRIES))
        .await;
    devices(&mut c).await.into_iter().find(|d| d.indexed_by_host == id).unwrap().id
}

/// Waits (15 s) until `q` finds exactly `want` files for `c`.
async fn wait_found(c: &mut Client, q: &str, want: u64) {
    let mut got = 0;
    for _ in 0..300 {
        got = search(c, q).await.1;
        if got == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("`{q}` finds {got} files, expected {want}");
}

/// The cursor of a PC's own drive.
fn own_cursor(pc: &Pc, device: DeviceId) -> Option<Cursor> {
    pc.state.own_snapshot(device).map(|(c, _)| c)
}

fn cursor_of(pc: &Pc, device: DeviceId) -> Option<Cursor> {
    pc.state.remote_cursors.lock().unwrap().get(&device).copied()
}

/// The in-memory copy of a device; a new one means a full copy was applied.
fn index_of(pc: &Pc, device: DeviceId) -> Option<SharedIndex> {
    pc.state.ensure_loaded(&[device], "test");
    pc.state.indexes.read().unwrap().get(&device).cloned()
}

fn same_copy(a: &Option<SharedIndex>, b: &Option<SharedIndex>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

async fn hub_cursor(hub: &TestHub, device: DeviceId, what: &str) -> Cursor {
    wait(what, || hub.store.cursor(device).is_some()).await;
    hub.store.cursor(device).unwrap()
}

/// Waits until the hub's newest cursor of `device` is past `than`.
async fn hub_cursor_past(hub: &TestHub, device: DeviceId, than: Cursor) -> Cursor {
    wait("the hub stores the change", || hub.store.cursor(device).is_some_and(|c| c != than)).await;
    hub.store.cursor(device).unwrap()
}

fn new_file(id: u64, name: &str) -> RawEntry {
    RawEntry { id, parent_id: 1, name: name.into(), size: 7, modified: 1_700_000_000_000, created: 1_700_000_000_000, attrs: 0 }
}

/// The volume of `fake` reports `entry` as new in `T:\folder1`.
fn add_file(fake: &Fake, entry: RawEntry) {
    *fake.folder_changes.lock().unwrap() = vec![Change::Upsert(entry)];
    let on_change = fake.on_change.lock().unwrap();
    let folder = std::path::PathBuf::from(r"T:\folder1");
    on_change.as_ref().expect("the PC watches its volume")(Changed::Dirs([folder].into_iter().collect()));
}

fn is_delta(served: &[Served], from: Cursor, to: Cursor) -> bool {
    matches!(served, [Served::Delta { from: f, to: t, .. }] if *f == from && *t == to)
}

/// The app tells a wrong code, a hub without an open window and an untrusted certificate apart by the prefix.
#[tokio::test(flavor = "multi_thread")]
async fn link_errors_have_a_stable_kind() {
    let hub = TestHub::start().await;
    let dir = tempfile::tempdir().unwrap();
    let pc = start_pc(dir.path(), "A", Fake::new("alpha", vec![]), None).await;
    let e = pc.rt().link_hub(&hub.addr(), "00000000", ME).await.unwrap_err();
    assert!(e.starts_with(seek_ipc::HUB_ERR_NO_WINDOW), "{e}");
    let code = hub.code();
    let wrong = if code == "00000000" { "11111111" } else { "00000000" };
    let e = pc.rt().link_hub(&hub.addr(), wrong, ME).await.unwrap_err();
    assert!(e.starts_with(seek_ipc::HUB_ERR_WRONG_CODE), "{e}");
    assert!(pc.state.peers.lock().unwrap().file.hub().is_none());
    pc.stop().await;
    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hub_sync_end_to_end() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut hub = TestHub::start().await;
    let (fa, fb) = (Fake::new("alpha", vec![volume(1)]), Fake::new("bravo", vec![volume(2)]));
    let a = start_pc(da.path(), "A", fa.clone(), None).await;
    let b = start_pc(db.path(), "B", fb.clone(), None).await;
    let (mut ca, mut cb) = (a.client().await, b.client().await);
    let (a_dev, b_dev) = (own_device(&a).await, own_device(&b).await);

    // both link; each finds the other's files
    hub.link(&a).await;
    hub.link(&b).await;
    wait_found(&mut cb, "alpha", FILES).await;
    wait_found(&mut ca, "bravo", FILES).await;
    assert_eq!(search(&mut cb, "bravo").await.1, FILES, "B keeps searching its own files");
    let status = a.rt().hub_status().expect("A is linked to the hub");
    assert_eq!((status.name.as_str(), status.pcs.len()), (HUB_NAME, 1));
    wait("A shows the hub online", || a.rt().hub_status().is_some_and(|s| s.online)).await;

    // a change on A reaches B as a delta
    let c0 = hub_cursor(&hub, a_dev, "the hub stores A's drive").await;
    wait("B holds A's drive at that cursor", || cursor_of(&b, a_dev) == Some(c0)).await;
    let copy0 = index_of(&b, a_dev);
    add_file(&fa, new_file(5000, "zzz_new.txt"));
    wait_found(&mut cb, "zzz_new", 1).await;
    let c1 = hub_cursor_past(&hub, a_dev, c0).await;
    assert_eq!(c1.generation, c0.generation, "a change continues the journal");
    assert!(is_delta(&hub.store.serve(a_dev, Some(c0)), c0, c1), "the hub stored a delta, not a new full copy");
    wait("B is at the new cursor", || cursor_of(&b, a_dev) == Some(c1)).await;
    assert!(same_copy(&copy0, &index_of(&b, a_dev)), "B applied the delta to its copy; it did not receive a Full");

    // B restarts while A changes again: B resumes with a delta. The hub is down while B starts, so the loaded
    // copy can be looked at before anything is pulled.
    let b_id = b.id();
    b.stop().await;
    add_file(&fa, new_file(5001, "yyy_second.txt"));
    let c2 = hub_cursor_past(&hub, a_dev, c1).await;
    assert_eq!(c2.generation, c1.generation);
    hub.stop().await;
    let b = start_pc(db.path(), "B", fb.clone(), None).await;
    assert_eq!(b.id(), b_id);
    let mut cb = b.client().await;
    assert_eq!(cursor_of(&b, a_dev), Some(c1), "B remembers how far it got");
    let loaded = index_of(&b, a_dev);
    assert!(loaded.is_some());
    assert_eq!(search(&mut cb, "zzz_new").await.1, 1, "and still has A's files while the hub is away");

    // the hub comes back on the same directories, key and port
    hub.restart().await;
    wait_found(&mut cb, "yyy_second", 1).await;
    wait("B is at the newest cursor", || cursor_of(&b, a_dev) == Some(c2)).await;
    assert!(same_copy(&loaded, &index_of(&b, a_dev)), "B resumed with a delta, not a full copy");

    // (B scanned its fake volume again when it started, so its drive is a new copy: wait for the upload)
    wait("every drive is stored as it is", || {
        hub.store.cursor(a_dev) == own_cursor(&a, a_dev) && hub.store.cursor(b_dev) == own_cursor(&b, b_dev)
    })
    .await;

    // a second restart: the hub keeps what it has, and the PCs find that out and upload nothing
    let (uploaded, stats) = (hub.copies(), hub.store.stats());
    let restarted = chrono::Utc::now().timestamp_millis();
    hub.restart().await;
    wait("both PCs pulled from the new hub", || {
        [&a, &b].iter().all(|pc| pc.rt().hub_status().is_some_and(|s| s.online && s.last_sync_ms > restarted))
    })
    .await;
    // (a PC uploads before it pulls, so everything it would upload has reached the hub by now)
    assert_eq!(hub.store.stats(), stats, "nothing was uploaded again after the hub restarted");
    assert_eq!(hub.copies(), uploaded, "no stored copy was touched");
    assert_eq!(hub.store.cursor(a_dev), Some(c2));

    // a full rescan on A: B gets one full copy
    let copy2 = index_of(&b, a_dev);
    assert_eq!(ca.request(Request::RescanDevice { id: a_dev }).await.unwrap(), Response::Ok);
    wait("B has a new copy of A's drive", || cursor_of(&b, a_dev).is_some_and(|c| c.generation != c2.generation))
        .await;
    let c3 = hub_cursor_past(&hub, a_dev, c2).await;
    assert_ne!(c3.generation, c2.generation);
    assert!(matches!(hub.store.serve(a_dev, Some(c2)).first(), Some(Served::Full { .. })), "the hub holds a new full copy");
    wait("B is at the new copy's cursor", || cursor_of(&b, a_dev) == Some(c3)).await;
    let copy3 = index_of(&b, a_dev);
    assert!(!same_copy(&copy2, &copy3), "B replaced its copy");
    wait_found(&mut cb, "alpha", FILES).await;
    wait_found(&mut cb, "yyy_second", 0).await; // the rescanned drive has only its own files
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(same_copy(&copy3, &index_of(&b, a_dev)), "and only once");

    // `seek-hub unlink` A: B drops A's drives at its next pull; A is told and goes back to the network
    assert!(hub.store.unlink_pc(a.id()).unwrap());
    hub.hub.close_session(a.id());
    wait_found(&mut cb, "alpha", 0).await;
    wait("B no longer holds A's drive", || index_of(&b, a_dev).is_none()).await;
    assert!(devices(&mut cb).await.iter().all(|d| d.indexed_by_host != a.id()));
    wait("A learns the hub unlinked it", || {
        a.rt().hub_status().is_some_and(|s| s.unlinked && !s.online)
    })
    .await;
    assert!(a.state.peers.lock().unwrap().file.hub().is_none(), "A is out of hub mode");
    wait("A listens on the local network again", || std::net::TcpStream::connect(a.rt().local_addr()).is_ok()).await;
    wait_found(&mut ca, "alpha", FILES).await;
    assert_eq!(search(&mut ca, "bravo").await.1, 0, "B's drives were received through the hub");
    // the notice goes away when dismissed
    a.rt().unlink_hub().await.unwrap();
    assert!(a.rt().hub_status().is_none());
    a.stop().await;
    b.stop().await;
    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hub_link_keeps_lan_copies_and_gets_deltas() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (fa, fb) = (Fake::new("alpha", vec![volume(1)]), Fake::new("bravo", vec![volume(2)]));
    let a = start_pc(da.path(), "A", fa.clone(), None).await;
    let b = start_pc(db.path(), "B", fb.clone(), None).await;
    let (mut ca, mut cb) = (a.client().await, b.client().await);
    let (a_dev, b_dev) = (own_device(&a).await, own_device(&b).await);

    // synced over the local network first
    link(&a, &b).await;
    wait_found(&mut cb, "alpha", FILES).await;
    wait_found(&mut ca, "bravo", FILES).await;
    let lan = cursor_of(&b, a_dev).expect("B has A's cursor from the LAN");
    assert!(cursor_of(&a, b_dev).is_some());
    let copy = index_of(&b, a_dev);

    // then both join the hub
    let hub = TestHub::start().await;
    hub.link(&a).await;
    assert_eq!(search(&mut cb, "alpha").await.1, FILES, "B keeps A's copy while A moves to the hub");
    hub.link(&b).await;
    assert_eq!(search(&mut cb, "alpha").await.1, FILES, "and after B moved too");
    assert_eq!(search(&mut ca, "bravo").await.1, FILES);

    // the hub's catalog adopts the copy; nothing is downloaded again
    let on_hub = hub_cursor(&hub, a_dev, "the hub stores A's drive").await;
    assert_eq!(on_hub, lan, "A uploaded the copy B already has");
    wait("B knows A through the hub", || {
        b.state.peers.lock().unwrap().get(a.id()).is_some_and(|p| p.via_hub)
            && a.state.peers.lock().unwrap().get(b.id()).is_some_and(|p| p.via_hub)
    })
    .await;
    assert!(same_copy(&copy, &index_of(&b, a_dev)), "B did not get a second copy of A's drive");
    assert_eq!(cursor_of(&b, a_dev), Some(lan));

    // a change on A arrives as a delta
    add_file(&fa, new_file(5000, "zzz_new.txt"));
    wait_found(&mut cb, "zzz_new", 1).await;
    let next = hub_cursor_past(&hub, a_dev, on_hub).await;
    assert_eq!(next.generation, lan.generation);
    assert!(is_delta(&hub.store.serve(a_dev, Some(lan)), lan, next), "the hub stored a delta");
    wait("B is at the new cursor", || cursor_of(&b, a_dev) == Some(next)).await;
    assert!(same_copy(&copy, &index_of(&b, a_dev)), "B received only the delta");
    a.stop().await;
    b.stop().await;
    hub.stop().await;
}

/// Terminates TLS on a loopback port (a certificate for `localhost`) and passes the bytes on to the hub, as
/// Tailscale Funnel does. Returns the port and the CA a PC has to trust.
async fn tls_front(hub: SocketAddr) -> (u16, Vec<u8>) {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(vec![]).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["localhost".to_string()]).unwrap().signed_by(&leaf_key, &ca, &ca_key).unwrap();
    let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![leaf.der().clone()], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())))
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else { return };
                let Ok(mut plain) = TcpStream::connect(hub).await else { return };
                let _ = tokio::io::copy_bidirectional(&mut tls, &mut plain).await;
            });
        }
    });
    (port, ca.der().to_vec())
}

#[tokio::test(flavor = "multi_thread")]
async fn hub_over_tls() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let hub = TestHub::start().await;
    let (port, ca) = tls_front(hub.sock()).await;
    let addr = format!("tls://localhost:{port}");
    let tls = TlsConfig::with_root(&ca);
    let (fa, fb) = (Fake::new("alpha", vec![volume(1)]), Fake::new("bravo", vec![volume(2)]));
    let a = start_pc(da.path(), "A", fa.clone(), Some(tls.clone())).await;
    let b = start_pc(db.path(), "B", fb, Some(tls)).await;
    let (mut ca, mut cb) = (a.client().await, b.client().await);
    let a_dev = own_device(&a).await;
    own_device(&b).await;

    hub.link_at(&a, &addr).await;
    hub.link_at(&b, &addr).await;
    assert_eq!(a.rt().hub_status().unwrap().addr, addr);
    wait_found(&mut cb, "alpha", FILES).await;
    wait_found(&mut ca, "bravo", FILES).await;

    // changes flow over the encrypted connection as well
    let c0 = hub_cursor(&hub, a_dev, "the hub stores A's drive").await;
    add_file(&fa, new_file(5000, "zzz_new.txt"));
    wait_found(&mut cb, "zzz_new", 1).await;
    assert!(hub_cursor_past(&hub, a_dev, c0).await.seq > c0.seq);

    // a PC that does not trust the certificate cannot link, and does not fall back to plain text
    let dc = tempfile::tempdir().unwrap();
    let c = start_pc(dc.path(), "C", Fake::new("charlie", vec![]), None).await;
    let e = c.rt().link_hub(&addr, &hub.code(), ME).await.unwrap_err();
    assert!(e.starts_with(seek_ipc::HUB_ERR_UNTRUSTED) && e.contains("not trusted"), "{e}");
    let plain = format!("localhost:{port}");
    assert!(c.rt().link_hub(&plain, &hub.code(), ME).await.is_err(), "the TLS port does not speak the plain protocol");
    assert!(c.state.peers.lock().unwrap().file.hub().is_none());
    c.stop().await;
    a.stop().await;
    b.stop().await;
    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn foreign_device_is_refused() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let hub = TestHub::start().await;
    let (fa, fb) = (Fake::new("alpha", vec![volume(1)]), Fake::new("bravo", vec![volume(2)]));
    let a = start_pc(da.path(), "A", fa, None).await;
    let b = start_pc(db.path(), "B", fb, None).await;
    let mut cb = b.client().await;
    let b_dev = own_device(&b).await;
    hub.link(&a).await;
    hub.link(&b).await;
    let held = hub_cursor(&hub, b_dev, "the hub stores B's drive").await;
    let stored = hub.copies();

    // a PC the hub knows, built by hand, claims B's drive
    let keys = Keys::generate();
    let mine = PairInfo {
        host_id: uuid::Uuid::new_v4(),
        name: "Mallory".into(),
        public_key: key_to_b64(&keys.public),
        user_sid: ME.into(),
        members: vec![],
        port: 0,
        role: Role::Pc,
        addrs: vec![],
        user_name: "mallory".into(),
    };
    let code = hub.code();
    let mut io = TcpStream::connect(hub.sock()).await.unwrap();
    io.write_all(&[seek_hub::net::PAIRING]).await.unwrap();
    let mut status = [0u8; 1];
    io.read_exact(&mut status).await.unwrap();
    let hub_info = pair_guest(&mut io, &code, &mine).await.expect("pairing");
    drop(io);
    wait("the hub has saved Mallory", || hub.store.pcs().iter().any(|p| p.host_id == mine.host_id)).await;
    let mut io = TcpStream::connect(hub.sock()).await.unwrap();
    io.write_all(&[seek_hub::net::SESSION]).await.unwrap();
    let (mut rd, mut wr) = noise::connect(io, &keys, &key_from_b64(&hub_info.public_key).unwrap()).await.unwrap();
    let hello = Msg::Hello { protocol: SYNC_PROTOCOL, host_id: mine.host_id, name: "Mallory".into(), role: Role::Pc };
    wr.send(&Frame::new(&hello, vec![])).await.unwrap();
    let reply: Msg = rd.recv().await.unwrap().msg().unwrap();
    assert!(matches!(reply, Msg::Hello { role: Role::Hub, .. }), "{reply:?}");

    let theirs: Device = devices(&mut cb).await.into_iter().find(|d| d.id == b_dev).unwrap();
    let not_owner = Msg::Refused { reason: Refusal::NotOwner { device: b_dev } };
    wr.send(&Frame::new(&Msg::Devices { devices: vec![theirs], profiles: vec![] }, vec![])).await.unwrap();
    assert_eq!(rd.recv().await.unwrap().msg::<Msg>().unwrap(), not_owner);
    wr.send(&Frame::new(&Msg::Full { device: b_dev, cursor: Cursor { generation: uuid::Uuid::new_v4(), seq: 0 } }, vec![1, 2, 3]))
        .await
        .unwrap();
    assert_eq!(rd.recv().await.unwrap().msg::<Msg>().unwrap(), not_owner);

    assert_eq!(hub.store.cursor(b_dev), Some(held), "B's copy on the hub is unchanged");
    assert_eq!(hub.copies(), stored, "and so are the files");
    assert!(hub.hub.counters().snapshot().ownership_violations >= 2);
    // B is not disturbed
    wait_found(&mut cb, "bravo", FILES).await;
    wait_found(&mut cb, "alpha", FILES).await;
    a.stop().await;
    b.stop().await;
    hub.stop().await;
}
