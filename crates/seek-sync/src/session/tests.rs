use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use seek_core::{
    decode_index, encode_index_level, synth_index, Change, Device, DeviceIdentity, DeviceIndex, DeviceKind,
    DeviceState, IndexHeader,
};

use super::*;
use crate::journal::{DeviceJournal, Since, JOURNAL_MAX_BYTES};
use crate::noise::{accept, connect, Keys};

struct Own {
    device: Device,
    ix: DeviceIndex,
    journal: DeviceJournal,
    shared: bool,
}

struct TestHost {
    id: uuid::Uuid,
    name: String,
    own: Mutex<HashMap<uuid::Uuid, Own>>,
    copies: Mutex<HashMap<uuid::Uuid, (Cursor, DeviceIndex)>>,
    remote: Mutex<Vec<Device>>,
    fulls: AtomicUsize,
    deltas: AtomicUsize,
    fail_next_apply: AtomicBool,
    unlinked: AtomicBool,
}

fn device(id: uuid::Uuid, host: uuid::Uuid, n: usize) -> Device {
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
        entry_count: n as u64,
        error: None,
    }
}

impl TestHost {
    fn new(name: &str) -> Arc<TestHost> {
        Arc::new(TestHost {
            id: uuid::Uuid::new_v4(),
            name: name.into(),
            own: Mutex::new(HashMap::new()),
            copies: Mutex::new(HashMap::new()),
            remote: Mutex::new(Vec::new()),
            fulls: AtomicUsize::new(0),
            deltas: AtomicUsize::new(0),
            fail_next_apply: AtomicBool::new(false),
            unlinked: AtomicBool::new(false),
        })
    }

    fn add_own_device(&self, n: usize) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
        let own = Own {
            device: device(id, self.id, n),
            ix: synth_index(n, id.as_u128() as u64),
            journal: DeviceJournal::in_memory(JOURNAL_MAX_BYTES),
            shared: true,
        };
        self.own.lock().unwrap().insert(id, own);
        id
    }

    fn change_own(&self, id: uuid::Uuid, changes: Vec<Change>) {
        let mut own = self.own.lock().unwrap();
        let o = own.get_mut(&id).unwrap();
        o.ix.apply(&changes);
        o.journal.record(&changes);
    }

    /// A full rescan: new index, new journal generation.
    fn reset_own(&self, id: uuid::Uuid, n: usize) {
        let mut own = self.own.lock().unwrap();
        let o = own.get_mut(&id).unwrap();
        o.ix = synth_index(n, 99);
        o.journal.reset();
    }

    fn set_shared(&self, id: uuid::Uuid, shared: bool) {
        self.own.lock().unwrap().get_mut(&id).unwrap().shared = shared;
    }

    fn copy_ids(&self) -> Vec<uuid::Uuid> {
        let mut v: Vec<_> = self.copies.lock().unwrap().keys().copied().collect();
        v.sort();
        v
    }

    async fn wait_copy(&self, id: uuid::Uuid, pred: impl Fn(&DeviceIndex) -> bool) {
        for _ in 0..500 {
            if self.copies.lock().unwrap().get(&id).is_some_and(|(_, ix)| pred(ix)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("copy of {id} never reached the expected state");
    }

    async fn wait(&self, what: &str, pred: impl Fn(&TestHost) -> bool) {
        for _ in 0..500 {
            if pred(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }
}

impl SyncHost for TestHost {
    fn host_id(&self) -> uuid::Uuid {
        self.id
    }
    fn name(&self) -> String {
        self.name.clone()
    }
    fn shared_devices(&self) -> Vec<Device> {
        let mut v: Vec<Device> =
            self.own.lock().unwrap().values().filter(|o| o.shared).map(|o| o.device.clone()).collect();
        v.sort_by_key(|d| d.id);
        v
    }
    fn profiles(&self) -> Vec<(String, String)> {
        vec![]
    }
    fn outgoing(&self, device: uuid::Uuid, have: Option<Cursor>) -> Option<Outgoing> {
        let own = self.own.lock().unwrap();
        let o = own.get(&device).filter(|o| o.shared)?;
        let since = match have {
            None => Since::Full,
            Some(c) => o.journal.since(&c),
        };
        match since {
            Since::UpToDate => None,
            Since::Delta(changes) => {
                self.deltas.fetch_add(1, Ordering::SeqCst);
                let c = o.journal.cursor();
                Some(Outgoing::Delta { from: have.unwrap(), to: c, changes })
            }
            Since::Full => {
                self.fulls.fetch_add(1, Ordering::SeqCst);
                let hdr = IndexHeader {
                    format_version: 0,
                    device_id: device,
                    host_id: self.id,
                    checkpoint: None,
                    saved_at: 0,
                    rows: 0,
                    sync_cursor: None,
                };
                let bytes = encode_index_level(&hdr, &mut o.ix.clone(), 1).unwrap();
                Some(Outgoing::Full { cursor: o.journal.cursor(), bytes })
            }
        }
    }
    fn held(&self, owner: uuid::Uuid) -> Vec<(uuid::Uuid, Cursor)> {
        let remote = self.remote.lock().unwrap();
        self.copies
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| remote.iter().any(|d| d.id == **id && d.indexed_by_host == owner))
            .map(|(id, (c, _))| (*id, *c))
            .collect()
    }
    fn remote_devices(&self, _owner: uuid::Uuid, devices: Vec<Device>, _profiles: Vec<(String, String)>) {
        self.copies.lock().unwrap().retain(|id, _| devices.iter().any(|d| d.id == *id));
        *self.remote.lock().unwrap() = devices;
    }
    fn apply_delta(
        &self,
        _owner: uuid::Uuid,
        device: uuid::Uuid,
        from: Cursor,
        to: Cursor,
        changes: Vec<Change>,
    ) -> Result<(), String> {
        if self.fail_next_apply.swap(false, Ordering::SeqCst) {
            return Err("injected failure".into());
        }
        let mut copies = self.copies.lock().unwrap();
        let (c, ix) = copies.get_mut(&device).ok_or("unknown copy")?;
        if *c != from {
            return Err("cursor mismatch".into());
        }
        ix.apply(&changes);
        *c = to;
        Ok(())
    }
    fn apply_full(&self, _owner: uuid::Uuid, device: uuid::Uuid, cursor: Cursor, bytes: Vec<u8>) -> Result<(), String> {
        let (_, ix) = decode_index(&bytes).map_err(|e| e.to_string())?;
        self.copies.lock().unwrap().insert(device, (cursor, ix));
        Ok(())
    }
    fn synced(&self, _peer: uuid::Uuid) {}
    fn member_joined(&self, _member: MemberInfo) {}
    fn unlinked_by(&self, _peer: uuid::Uuid) {
        self.unlinked.store(true, Ordering::SeqCst);
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

/// Connects A (dialer) and B (listener) over an in-memory Noise channel and runs both sessions.
async fn connect_pair(a: Arc<TestHost>, b: Arc<TestHost>, interval: Duration) -> Pair {
    let (ka, kb) = (Keys::generate(), Keys::generate());
    let (ia, ib) = tokio::io::duplex(1 << 16);
    let kb2 = kb.clone();
    let accepting = tokio::spawn(async move { accept(ib, &kb2, |_| true).await.unwrap() });
    let (ra, wa) = connect(ia, &ka, &kb.public).await.unwrap();
    let (_, rb, wb) = accepting.await.unwrap();
    let (ctl_a, out_a) = SessionCtl::new();
    let (ctl_b, out_b) = SessionCtl::new();
    let (ida, idb) = (a.id, b.id);
    let ta = tokio::spawn(run_session(a, idb, ra, wa, ctl_a.clone(), out_a, interval));
    let tb = tokio::spawn(run_session(b, ida, rb, wb, ctl_b.clone(), out_b, interval));
    Pair { a: ctl_a, b: ctl_b, ta, tb }
}

const LONG: Duration = Duration::from_secs(3600);

#[tokio::test]
async fn first_sync_is_full_then_only_deltas() {
    let (a, b) = (TestHost::new("A"), TestHost::new("B"));
    let dev = a.add_own_device(5_000);
    let p = connect_pair(a.clone(), b.clone(), LONG).await;
    b.wait_copy(dev, |ix| ix.live_count() == 5_000).await;
    assert_eq!(a.fulls.load(Ordering::SeqCst), 1);
    a.change_own(dev, vec![Change::Delete { id: 142 }]);
    p.a.sync_now.notify_one();
    b.wait_copy(dev, |ix| ix.row_of(142).is_none_or(|r| !ix.is_alive(r))).await;
    assert_eq!(a.fulls.load(Ordering::SeqCst), 1, "second update must be a delta");
    assert_eq!(a.deltas.load(Ordering::SeqCst), 1);
    let cursor = b.copies.lock().unwrap()[&dev].0;
    assert_eq!(cursor, a.own.lock().unwrap()[&dev].journal.cursor());
    p.stop().await;
}

#[tokio::test]
async fn periodic_tick_pushes_changes() {
    let (a, b) = (TestHost::new("A"), TestHost::new("B"));
    let dev = a.add_own_device(1_000);
    let p = connect_pair(a.clone(), b.clone(), Duration::from_millis(100)).await;
    b.wait_copy(dev, |ix| ix.live_count() == 1_000).await;
    a.change_own(dev, vec![Change::Delete { id: 150 }]);
    b.wait_copy(dev, |ix| ix.row_of(150).is_none_or(|r| !ix.is_alive(r))).await;
    p.stop().await;
}

#[tokio::test]
async fn reconnect_resumes_with_delta_and_rescan_forces_full() {
    let (a, b) = (TestHost::new("A"), TestHost::new("B"));
    let dev = a.add_own_device(2_000);
    let p = connect_pair(a.clone(), b.clone(), LONG).await;
    b.wait_copy(dev, |ix| ix.live_count() == 2_000).await;
    p.stop().await;

    a.change_own(dev, vec![Change::Delete { id: 160 }]);
    let p = connect_pair(a.clone(), b.clone(), LONG).await;
    b.wait_copy(dev, |ix| ix.row_of(160).is_none_or(|r| !ix.is_alive(r))).await;
    assert_eq!(a.fulls.load(Ordering::SeqCst), 1);
    assert_eq!(a.deltas.load(Ordering::SeqCst), 1);

    a.reset_own(dev, 300);
    p.a.sync_now.notify_one();
    b.wait_copy(dev, |ix| ix.live_count() == 300).await;
    assert_eq!(a.fulls.load(Ordering::SeqCst), 2);
    p.stop().await;
}

#[tokio::test]
async fn sync_now_from_receiver_pulls_owner_changes() {
    let (a, b) = (TestHost::new("A"), TestHost::new("B"));
    let dev = a.add_own_device(1_000);
    let p = connect_pair(a.clone(), b.clone(), LONG).await;
    b.wait_copy(dev, |ix| ix.live_count() == 1_000).await;
    a.change_own(dev, vec![Change::Delete { id: 170 }]);
    p.b.sync_now.notify_one();
    b.wait_copy(dev, |ix| ix.row_of(170).is_none_or(|r| !ix.is_alive(r))).await;
    p.stop().await;
}

#[tokio::test]
async fn both_directions_and_unshared_device_is_removed() {
    let (a, b) = (TestHost::new("A"), TestHost::new("B"));
    let d1 = a.add_own_device(500);
    let d2 = a.add_own_device(600);
    let db = b.add_own_device(700);
    let p = connect_pair(a.clone(), b.clone(), LONG).await;
    b.wait_copy(d1, |ix| ix.live_count() == 500).await;
    b.wait_copy(d2, |ix| ix.live_count() == 600).await;
    a.wait_copy(db, |ix| ix.live_count() == 700).await;
    a.set_shared(d2, false);
    p.a.sync_now.notify_one();
    b.wait("d2 removed", |h| h.copy_ids() == vec![d1]).await;
    p.stop().await;
}

#[tokio::test]
async fn dropped_connection_keeps_old_copy() {
    let (a, b) = (TestHost::new("A"), TestHost::new("B"));
    let dev = a.add_own_device(1_000);
    let p = connect_pair(a.clone(), b.clone(), LONG).await;
    b.wait_copy(dev, |ix| ix.live_count() == 1_000).await;
    let old = b.copies.lock().unwrap()[&dev].0;
    p.a.stop.notify_one();
    let _ = p.ta.await;
    let _ = p.tb.await;
    a.change_own(dev, vec![Change::Delete { id: 180 }]);
    assert_eq!(b.copies.lock().unwrap()[&dev].0, old);
    assert!(b.copies.lock().unwrap()[&dev].1.row_of(180).is_some());
    let p = connect_pair(a.clone(), b.clone(), LONG).await;
    b.wait_copy(dev, |ix| ix.row_of(180).is_none_or(|r| !ix.is_alive(r))).await;
    assert_eq!(a.fulls.load(Ordering::SeqCst), 1);
    p.stop().await;
}

#[tokio::test]
async fn failed_delta_triggers_full_copy() {
    let (a, b) = (TestHost::new("A"), TestHost::new("B"));
    let dev = a.add_own_device(1_000);
    let p = connect_pair(a.clone(), b.clone(), LONG).await;
    b.wait_copy(dev, |ix| ix.live_count() == 1_000).await;
    b.fail_next_apply.store(true, Ordering::SeqCst);
    a.change_own(dev, vec![Change::Delete { id: 190 }]);
    p.a.sync_now.notify_one();
    b.wait_copy(dev, |ix| ix.row_of(190).is_none_or(|r| !ix.is_alive(r))).await;
    assert_eq!(a.fulls.load(Ordering::SeqCst), 2);
    p.stop().await;
}

#[tokio::test]
async fn unlink_reaches_the_peer_and_ends_both_sessions() {
    let (a, b) = (TestHost::new("A"), TestHost::new("B"));
    a.add_own_device(10);
    let p = connect_pair(a.clone(), b.clone(), LONG).await;
    p.a.send(Msg::Unlinked);
    assert!(p.ta.await.unwrap().is_ok());
    assert!(p.tb.await.unwrap().is_ok());
    assert!(b.unlinked.load(Ordering::SeqCst));
}

#[test]
fn delta_payload_roundtrip_and_garbage() {
    let ch = vec![Change::Delete { id: 9 }];
    assert_eq!(decode_delta(&encode_delta(&ch)).unwrap(), ch);
    assert!(decode_delta(b"nope").is_err());
}
