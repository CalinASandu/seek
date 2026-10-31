use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use seek_core::{encode_index, synth_index, Change, Device, DeviceIdentity, DeviceKind, DeviceState, IndexHeader, RawEntry};
use seek_sync::noise::{connect, Keys};
use seek_sync::session::encode_delta;
use seek_sync::{CatalogPc, Cursor, Refusal};
use tokio::io::{DuplexStream, WriteHalf};

use super::*;
use crate::config::HubConfig;
use crate::crypt::HubKey;
use crate::net::Counters;
use crate::store::{PcRecord, Served};

pub(crate) fn device(id: Uuid, host: Uuid) -> Device {
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

fn cur(seq: u64) -> Cursor {
    Cursor { generation: Uuid::from_u128(77), seq }
}

/// An encoded `.seekidx` of a small synthetic drive (compaction decodes it).
fn full_bytes(device: Uuid, host: Uuid) -> Vec<u8> {
    let hdr = IndexHeader {
        format_version: 0,
        device_id: device,
        host_id: host,
        checkpoint: None,
        saved_at: 0,
        rows: 0,
        sync_cursor: None,
    };
    encode_index(&hdr, &mut synth_index(50, 3)).unwrap()
}

/// A valid delta payload of `n` new files.
fn delta(n: u64) -> Vec<u8> {
    let changes: Vec<Change> = (0..n)
        .map(|i| {
            let name = format!("added file number {i} with a long name {}.txt", i * 7919);
            Change::Upsert(RawEntry { id: 50_000 + i, parent_id: 1, name, size: 1, modified: 0, created: 0, attrs: 0 })
        })
        .collect();
    encode_delta(&changes)
}

/// A hub store with two linked PCs, A and B; B owns drive `b_drive`.
struct Env {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    a: (PcRecord, Keys),
    b: (PcRecord, Keys),
    b_drive: Uuid,
}

fn linked_pc(n: u128) -> (PcRecord, Keys) {
    let keys = Keys::generate();
    let rec = PcRecord {
        host_id: Uuid::from_u128(n),
        name: format!("PC {n}"),
        public_key: seek_sync::peers::key_to_b64(&keys.public),
        user_sid: format!("S-1-{n}"),
        user_name: format!("user{n}"),
        profiles: vec![],
        last_seen_ms: 0,
    };
    (rec, keys)
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let cfg = HubConfig {
        name: "Test Hub".into(),
        listen: vec!["127.0.0.1:0".parse().unwrap()],
        public: None,
        data_dir: dir.path().join("data"),
        keys_dir: dir.path().join("keys"),
        pc_quota_bytes: 2048 * 1024 * 1024,
        pc_max_devices: 64,
    };
    let key = HubKey::create(&cfg.keys_dir).unwrap();
    let store = Arc::new(Store::open(&cfg, key).unwrap());
    let (a, b) = (linked_pc(1), linked_pc(2));
    store.add_pc(a.0.clone()).unwrap();
    store.add_pc(b.0.clone()).unwrap();
    let b_drive = Uuid::from_u128(20);
    store.set_pc_devices(b.0.host_id, vec![device(b_drive, b.0.host_id)], vec![]).unwrap();
    Env { _dir: dir, store, a, b, b_drive }
}

impl Env {
    fn a_id(&self) -> Uuid {
        self.a.0.host_id
    }
}

/// One frame the hub wrote, with how many frames the PC had sent when it arrived.
type Written = (usize, Msg, Vec<u8>);

/// The PC end of a hub session. The PC works in lockstep (it sends the next frame only after the replies to
/// the last one arrived), so a reply to its `k`-th frame is recorded with `k`; a frame the hub wrote on its
/// own would show up with a stale count, or after the scenario's last reply.
struct Pc {
    id: Uuid,
    wr: SecureWriter<WriteHalf<DuplexStream>>,
    sent: Arc<AtomicUsize>,
    got: Arc<Mutex<Vec<Written>>>,
    counters: Arc<Counters>,
    hub: tokio::task::JoinHandle<Result<(), Error>>,
    reader: tokio::task::JoinHandle<()>,
}

impl Pc {
    /// Connects as `pc` and starts the hub's side with `serve_pc`.
    async fn start(store: &Arc<Store>, pc: &(PcRecord, Keys)) -> Pc {
        let (ip, ih) = tokio::io::duplex(1 << 16);
        let hub_keys = store.noise_keys();
        let hub_public = hub_keys.public;
        let accepting = tokio::spawn(async move { seek_sync::noise::accept(ih, &hub_keys, |_| true).await.unwrap() });
        let (mut rd, wr) = connect(ip, &pc.1, &hub_public).await.unwrap();
        let (_, hrd, hwr) = accepting.await.unwrap();
        let id = pc.0.host_id;
        let counters = Arc::new(Counters::default());
        let (s, c) = (store.clone(), counters.clone());
        let hub = tokio::spawn(async move { serve_pc(s, &c, id, hrd, hwr).await });
        let sent = Arc::new(AtomicUsize::new(0));
        let got = Arc::new(Mutex::new(Vec::new()));
        let (s, g) = (sent.clone(), got.clone());
        let reader = tokio::spawn(async move {
            while let Ok(f) = rd.recv().await {
                let m: Msg = f.msg().unwrap();
                g.lock().unwrap().push((s.load(Ordering::SeqCst), m, f.payload));
            }
        });
        Pc { id, wr, sent, got, counters, hub, reader }
    }

    fn written(&self) -> usize {
        self.got.lock().unwrap().len()
    }

    /// Sends one frame and returns the `replies` frames the hub answers it with. With `replies == 0`, gives
    /// the hub a moment to (wrongly) answer anyway.
    async fn ask(&mut self, m: Msg, payload: Vec<u8>, replies: usize) -> Vec<(Msg, Vec<u8>)> {
        let before = self.written();
        let k = self.sent.fetch_add(1, Ordering::SeqCst) + 1;
        self.wr.send(&Frame::new(&m, payload)).await.unwrap();
        if replies == 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while self.written() < before + replies {
            assert!(std::time::Instant::now() < deadline, "hub did not answer {m:?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let got = self.got.lock().unwrap();
        assert_eq!(got.len(), before + replies, "replies to {m:?}: {:?}", &got[before..]);
        got[before..]
            .iter()
            .map(|(n, msg, p)| {
                assert_eq!(*n, k, "{msg:?} is not a reply to frame {k} ({m:?})");
                (msg.clone(), p.clone())
            })
            .collect()
    }

    async fn hello(&mut self) {
        let hello = Msg::Hello { protocol: SYNC_PROTOCOL, host_id: self.id, name: "PC".into(), role: Role::Pc };
        let r = self.ask(hello, vec![], 1).await;
        assert!(matches!(&r[0].0, Msg::Hello { protocol: SYNC_PROTOCOL, role: Role::Hub, .. }), "{r:?}");
    }

    /// Closes the connection (both halves of the stream); the hub's side ends.
    async fn finish(self) -> Result<(), Error> {
        self.reader.abort();
        let _ = self.reader.await;
        drop(self.wr);
        tokio::time::timeout(Duration::from_secs(10), self.hub).await.expect("hub session did not end").unwrap()
    }
}

fn msgs(r: &[(Msg, Vec<u8>)]) -> Vec<Msg> {
    r.iter().map(|(m, _)| m.clone()).collect()
}

#[tokio::test]
async fn every_hub_frame_is_a_reply() {
    let e = env();
    let bd = e.b_drive;
    e.store.put_full(e.b.0.host_id, bd, cur(0), full_bytes(bd, e.b.0.host_id)).unwrap();
    let ad = Uuid::from_u128(10);
    let mut pc = Pc::start(&e.store, &e.a).await;

    pc.hello().await;
    let devices = Msg::Devices { devices: vec![device(ad, e.a_id())], profiles: vec![] };
    assert_eq!(msgs(&pc.ask(devices, vec![], 1).await), vec![Msg::Have { cursors: vec![] }]);
    let full = Msg::Full { device: ad, cursor: cur(0) };
    assert_eq!(msgs(&pc.ask(full, full_bytes(ad, e.a_id()), 1).await), vec![Msg::Ack { device: ad, cursor: cur(0) }]);
    let d = Msg::Delta { device: ad, from: cur(0), to: cur(1) };
    assert_eq!(msgs(&pc.ask(d, delta(3), 1).await), vec![Msg::Ack { device: ad, cursor: cur(1) }]);
    let pulled = pc.ask(Msg::Pull { cursors: vec![] }, vec![], 3).await;
    assert!(matches!(&pulled[0].0, Msg::Catalog { .. }));
    assert_eq!(pulled[1].0, Msg::Full { device: bd, cursor: cur(0) });
    assert_eq!(pulled[2].0, Msg::PullDone);
    pc.ask(Msg::Ack { device: bd, cursor: cur(0) }, vec![], 0).await;
    pc.ask(Msg::SyncNow, vec![], 0).await;

    // five idle minutes: the hub has nothing to answer, so it writes nothing
    let before = pc.written();
    tokio::time::pause();
    tokio::time::sleep(Duration::from_secs(5 * 60)).await;
    tokio::time::resume();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(pc.written(), before, "the hub wrote during the idle stretch");

    let got: Vec<usize> = pc.got.lock().unwrap().iter().map(|(k, ..)| *k).collect();
    assert_eq!(got, vec![1, 2, 3, 4, 5, 5, 5]);
    pc.finish().await.unwrap();
    let a = e.store.pcs().into_iter().find(|p| p.host_id == e.a_id()).unwrap();
    assert!(a.last_seen_ms > 0, "activity recorded");
    assert_eq!(e.store.cursor(ad), Some(cur(1)));
}

#[tokio::test]
async fn pull_returns_catalog_copies_done() {
    let e = env();
    let (b, bd) = (e.b.0.host_id, e.b_drive);
    e.store.put_full(b, bd, cur(0), full_bytes(bd, b)).unwrap();
    e.store.put_delta(b, bd, cur(0), cur(2), delta(2)).unwrap();
    let ad = Uuid::from_u128(10);
    let mut pc = Pc::start(&e.store, &e.a).await;
    pc.hello().await;
    let devices = Msg::Devices { devices: vec![device(ad, e.a_id())], profiles: vec![] };
    pc.ask(devices, vec![], 1).await;

    // nothing held: catalog, the full copy, the delta from it to the newest, done
    let r = pc.ask(Msg::Pull { cursors: vec![] }, vec![], 4).await;
    let Msg::Catalog { pcs, devices } = &r[0].0 else { panic!("expected catalog, got {:?}", r[0].0) };
    let hosts: Vec<Uuid> = pcs.iter().map(|p: &CatalogPc| p.host_id).collect();
    assert_eq!(hosts, vec![b], "every other PC, not the asking one");
    assert_eq!(devices.iter().map(|d| d.id).collect::<Vec<_>>(), vec![bd]);
    assert_eq!(r[1], (Msg::Full { device: bd, cursor: cur(0) }, full_bytes(bd, b)));
    assert_eq!(r[2].0, Msg::Delta { device: bd, from: cur(0), to: cur(2) });
    assert_eq!(r[2].1, delta(2));
    assert_eq!(r[3].0, Msg::PullDone);

    // up to date: catalog and done only
    let r = pc.ask(Msg::Pull { cursors: vec![(bd, cur(2))] }, vec![], 2).await;
    assert!(matches!(r[0].0, Msg::Catalog { .. }));
    assert_eq!(r[1].0, Msg::PullDone);
    // held at the full copy's cursor: one delta
    let r = pc.ask(Msg::Pull { cursors: vec![(bd, cur(0))] }, vec![], 3).await;
    assert_eq!(r[1].0, Msg::Delta { device: bd, from: cur(0), to: cur(2) });
    assert_eq!(r[2].0, Msg::PullDone);
    // a copy the PC could not apply is reported as `Cursor::none()`: served in full again
    let r = pc.ask(Msg::Pull { cursors: vec![(bd, Cursor::none())] }, vec![], 4).await;
    assert_eq!(r[1].0, Msg::Full { device: bd, cursor: cur(0) });
    pc.finish().await.unwrap();
}

#[tokio::test]
async fn devices_answered_with_stored_cursors() {
    let e = env();
    let ad = Uuid::from_u128(10);
    let mut pc = Pc::start(&e.store, &e.a).await;
    pc.hello().await;
    let a = e.a_id();
    let devices = || Msg::Devices { devices: vec![device(ad, a)], profiles: vec![] };
    pc.ask(devices(), vec![], 1).await;
    pc.ask(Msg::Full { device: ad, cursor: cur(4) }, full_bytes(ad, e.a_id()), 1).await;
    assert_eq!(msgs(&pc.ask(devices(), vec![], 1).await), vec![Msg::Have { cursors: vec![(ad, cur(4))] }]);
    // a delta that does not continue the stored copy: the hub asks for a full one
    let d = Msg::Delta { device: ad, from: cur(9), to: cur(10) };
    assert_eq!(msgs(&pc.ask(d, delta(1), 1).await), vec![Msg::Have { cursors: vec![(ad, Cursor::none())] }]);
    pc.finish().await.unwrap();
}

#[tokio::test]
async fn foreign_device_refused() {
    let e = env();
    let bd = e.b_drive;
    let mut pc = Pc::start(&e.store, &e.a).await;
    pc.hello().await;
    let not_owner = Msg::Refused { reason: Refusal::NotOwner { device: bd } };
    // claiming B's drive in the device list, as if A had indexed it
    let devices = Msg::Devices { devices: vec![device(bd, e.a_id())], profiles: vec![] };
    assert_eq!(msgs(&pc.ask(devices, vec![], 1).await), vec![not_owner.clone()]);
    let full = Msg::Full { device: bd, cursor: cur(0) };
    assert_eq!(msgs(&pc.ask(full, full_bytes(bd, e.a_id()), 1).await), vec![not_owner.clone()]);
    let d = Msg::Delta { device: bd, from: cur(0), to: cur(1) };
    assert_eq!(msgs(&pc.ask(d, delta(1), 1).await), vec![not_owner]);
    assert_eq!(e.store.cursor(bd), None, "nothing stored for B's drive");
    let c = pc.counters.snapshot();
    assert_eq!((c.ownership_violations, c.quota_hits), (3, 0));
    pc.finish().await.unwrap();
}

#[tokio::test]
async fn quota_refusals_counted() {
    let e = env();
    let mut pc = Pc::start(&e.store, &e.a).await;
    pc.hello().await;
    // one drive over the 64 a PC may list
    let a = e.a_id();
    let many = (0..65).map(|n| device(Uuid::from_u128(1000 + n), a)).collect();
    let r = pc.ask(Msg::Devices { devices: many, profiles: vec![] }, vec![], 1).await;
    assert_eq!(msgs(&r), vec![Msg::Refused { reason: Refusal::TooManyDevices }]);
    let c = pc.counters.snapshot();
    assert_eq!((c.ownership_violations, c.quota_hits), (0, 1));
    pc.finish().await.unwrap();
}

#[tokio::test]
async fn hub_full_counted() {
    let e = env();
    let ad = Uuid::from_u128(10);
    let mut pc = Pc::start(&e.store, &e.a).await;
    pc.hello().await;
    pc.ask(Msg::Devices { devices: vec![device(ad, e.a_id())], profiles: vec![] }, vec![], 1).await;
    // the disk refuses every write
    e.store.set_fault(Some(Box::new(|write, _| write.then(|| std::io::Error::other("disk full")))));
    let full = Msg::Full { device: ad, cursor: cur(0) };
    let r = pc.ask(full, full_bytes(ad, e.a_id()), 1).await;
    assert_eq!(msgs(&r), vec![Msg::Refused { reason: Refusal::HubFull }]);
    assert_eq!(pc.counters.snapshot().quota_hits, 1);
    e.store.set_fault(None);
    pc.finish().await.unwrap();
}

#[tokio::test]
async fn open_refused_not_supported() {
    let e = env();
    let mut pc = Pc::start(&e.store, &e.a).await;
    pc.hello().await;
    let open = Msg::Open { channel: 1, to: e.b.0.host_id };
    assert_eq!(msgs(&pc.ask(open, vec![], 1).await), vec![Msg::Refused { reason: Refusal::NotSupported }]);
    // the rest of the reserved relay messages and things a hub never takes go unanswered
    pc.ask(Msg::Data { channel: 1 }, vec![1, 2], 0).await;
    pc.ask(Msg::Close { channel: 1 }, vec![], 0).await;
    pc.ask(Msg::Have { cursors: vec![] }, vec![], 0).await;
    pc.finish().await.unwrap();
}

#[tokio::test]
async fn unlinked_from_pc_deletes() {
    let e = env();
    let ad = Uuid::from_u128(10);
    let mut pc = Pc::start(&e.store, &e.a).await;
    pc.hello().await;
    pc.ask(Msg::Devices { devices: vec![device(ad, e.a_id())], profiles: vec![] }, vec![], 1).await;
    pc.ask(Msg::Full { device: ad, cursor: cur(0) }, full_bytes(ad, e.a_id()), 1).await;
    pc.ask(Msg::Unlinked, vec![], 0).await;
    let hub = pc.hub;
    let r = tokio::time::timeout(Duration::from_secs(10), hub).await.expect("session ended").unwrap();
    assert!(r.is_ok(), "{r:?}");
    assert!(e.store.pcs().iter().all(|p| p.host_id != e.a_id()));
    assert_eq!(e.store.cursor(ad), None);
    assert!(e.store.is_tombstoned(&e.a.1.public));
    assert!(e.store.pcs().iter().any(|p| p.host_id == e.b.0.host_id), "other PCs stay");
}

#[tokio::test]
async fn protocol_1_closed() {
    let e = env();
    let id = e.a_id();
    let hellos = [
        Msg::Hello { protocol: 1, host_id: id, name: "old".into(), role: Role::Pc },
        // a hello naming another PC than the key that authenticated
        Msg::Hello { protocol: SYNC_PROTOCOL, host_id: e.b.0.host_id, name: "PC".into(), role: Role::Pc },
        // anything but a hello first
        Msg::Pull { cursors: vec![] },
    ];
    for first in hellos {
        let mut pc = Pc::start(&e.store, &e.a).await;
        pc.wr.send(&Frame::new(&first, vec![])).await.unwrap();
        let hub = pc.hub;
        let r = tokio::time::timeout(Duration::from_secs(10), hub).await.expect("session ended").unwrap();
        assert!(r.is_err(), "{first:?} accepted");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(pc.got.lock().unwrap().len(), 0, "nothing written after {first:?}");
    }
}

#[tokio::test]
async fn compaction_runs_after_a_delta() {
    let e = env();
    let ad = Uuid::from_u128(10);
    let mut pc = Pc::start(&e.store, &e.a).await;
    pc.hello().await;
    pc.ask(Msg::Devices { devices: vec![device(ad, e.a_id())], profiles: vec![] }, vec![], 1).await;
    pc.ask(Msg::Full { device: ad, cursor: cur(0) }, full_bytes(ad, e.a_id()), 1).await;
    // a batch far over 25 % of the full copy
    let d = Msg::Delta { device: ad, from: cur(0), to: cur(1) };
    assert_eq!(msgs(&pc.ask(d, delta(400), 1).await), vec![Msg::Ack { device: ad, cursor: cur(1) }]);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while e.store.serve(ad, None).len() != 1 {
        assert!(std::time::Instant::now() < deadline, "journal never compacted");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(e.store.compact_due().is_empty());
    assert!(matches!(&e.store.serve(ad, None)[0], Served::Full { cursor, .. } if *cursor == cur(1)));
    pc.finish().await.unwrap();
}

#[tokio::test]
async fn reply_unlinked_sends_one_frame() {
    let (ip, ih) = tokio::io::duplex(1 << 16);
    let (kp, kh) = (Keys::generate(), Keys::generate());
    let kh2 = kh.clone();
    let accepting = tokio::spawn(async move { seek_sync::noise::accept(ih, &kh2, |_| true).await.unwrap() });
    let (mut rd, _wr) = connect(ip, &kp, &kh.public).await.unwrap();
    let (_, _hrd, hwr) = accepting.await.unwrap();
    reply_unlinked(hwr).await.unwrap();
    assert_eq!(rd.recv().await.unwrap().msg::<Msg>().unwrap(), Msg::Unlinked);
}

#[tokio::test]
async fn a_huge_json_part_ends_the_session() {
    let e = env();
    let mut pc = Pc::start(&e.store, &e.a).await;
    pc.hello().await;
    let big = Msg::Hello { protocol: SYNC_PROTOCOL, host_id: pc.id, name: "x".repeat(5 << 20), role: Role::Pc };
    pc.wr.send(&Frame::new(&big, vec![])).await.ok();
    let r = tokio::time::timeout(Duration::from_secs(10), pc.hub).await.expect("session ended").unwrap();
    assert!(matches!(r, Err(Error::Protocol(m)) if m.contains("header")));
}

#[test]
fn frame_limit_is_the_quota_capped_at_256_mib() {
    let e = env();
    assert_eq!(e.store.max_frame(), 256 << 20);
}
