use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use seek_core::{Change, Device, DeviceIdentity, DeviceKind, DeviceState};
use tokio::io::{DuplexStream, ReadHalf, WriteHalf};

use super::*;
use crate::journal::Cursor;
use crate::noise::{accept, connect, Keys};
use crate::wire::{CatalogPc, Refusal};
use crate::pairing::MemberInfo;
use crate::session::Outgoing;

fn device(id: uuid::Uuid, host: uuid::Uuid) -> Device {
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

fn cursor(seq: u64) -> Cursor {
    Cursor { generation: uuid::Uuid::from_u128(7), seq }
}

/// `(hub, pcs, devices)` of one `SyncHost::catalog` call.
type CatalogCall = (uuid::Uuid, Vec<CatalogPc>, Vec<Device>);

/// A PC with own devices whose full copies are three bytes; records what the hub session does to it.
struct Fake {
    id: uuid::Uuid,
    own: Vec<Device>,
    held: Vec<(uuid::Uuid, Cursor)>,
    catalogs: Mutex<Vec<CatalogCall>>,
    /// `(owner, device, cursor)` of every applied full copy.
    fulls: Mutex<Vec<(uuid::Uuid, uuid::Uuid, Cursor)>>,
    refused: Mutex<Vec<Refusal>>,
    unlinked: AtomicBool,
    fail_next_apply: AtomicBool,
    /// Offer a full copy of every own device at each push, not only the first.
    full_always: AtomicBool,
    /// Own devices added during a test.
    extra: Mutex<Vec<Device>>,
}

impl Fake {
    fn new(own_devices: usize) -> Arc<Fake> {
        let id = uuid::Uuid::new_v4();
        Arc::new(Fake {
            id,
            own: (0..own_devices).map(|_| device(uuid::Uuid::new_v4(), id)).collect(),
            held: vec![(uuid::Uuid::from_u128(42), cursor(3))],
            catalogs: Mutex::new(Vec::new()),
            fulls: Mutex::new(Vec::new()),
            refused: Mutex::new(Vec::new()),
            unlinked: AtomicBool::new(false),
            fail_next_apply: AtomicBool::new(false),
            full_always: AtomicBool::new(false),
            extra: Mutex::new(Vec::new()),
        })
    }
}

impl SyncHost for Fake {
    fn host_id(&self) -> uuid::Uuid {
        self.id
    }
    fn name(&self) -> String {
        "PC A".into()
    }
    fn shared_devices(&self) -> Vec<Device> {
        let mut all = self.own.clone();
        all.extend(self.extra.lock().unwrap().iter().cloned());
        all
    }
    fn profiles(&self) -> Vec<(String, String)> {
        vec![]
    }
    fn outgoing(&self, device: uuid::Uuid, have: Option<Cursor>) -> Option<Outgoing> {
        let own = self.own.iter().chain(self.extra.lock().unwrap().iter()).any(|d| d.id == device);
        (own && (have.is_none() || self.full_always.load(Ordering::SeqCst))).then(|| Outgoing::Full { cursor: cursor(1), bytes: vec![1, 2, 3] })
    }
    fn held(&self, _owner: uuid::Uuid) -> Vec<(uuid::Uuid, Cursor)> {
        vec![]
    }
    fn held_remote(&self) -> Vec<(uuid::Uuid, Cursor)> {
        self.held.clone()
    }
    fn remote_devices(&self, _owner: uuid::Uuid, _devices: Vec<Device>, _profiles: Vec<(String, String)>) {}
    fn catalog(&self, hub: uuid::Uuid, pcs: Vec<CatalogPc>, devices: Vec<Device>) {
        self.catalogs.lock().unwrap().push((hub, pcs, devices));
    }
    fn apply_delta(
        &self,
        _owner: uuid::Uuid,
        _device: uuid::Uuid,
        _from: Cursor,
        _to: Cursor,
        _changes: Vec<Change>,
    ) -> Result<(), String> {
        Err("not expected".into())
    }
    fn apply_full(&self, owner: uuid::Uuid, device: uuid::Uuid, cursor: Cursor, _bytes: Vec<u8>) -> Result<(), String> {
        if self.fail_next_apply.swap(false, Ordering::SeqCst) {
            return Err("injected failure".into());
        }
        self.fulls.lock().unwrap().push((owner, device, cursor));
        Ok(())
    }
    fn synced(&self, _peer: uuid::Uuid) {}
    fn member_joined(&self, _member: MemberInfo) {}
    fn unlinked_by(&self, _peer: uuid::Uuid) {
        self.unlinked.store(true, Ordering::SeqCst);
    }
    fn hub_refused(&self, reason: Refusal) {
        self.refused.lock().unwrap().push(reason);
    }
}

/// The hub end of a session, driven by hand with raw frames.
struct FakeHub {
    id: uuid::Uuid,
    rd: SecureReader<ReadHalf<DuplexStream>>,
    wr: SecureWriter<WriteHalf<DuplexStream>>,
    ctl: Arc<SessionCtl>,
    task: tokio::task::JoinHandle<Result<(), Error>>,
}

impl FakeHub {
    /// Connects `pc` to a fake hub and starts its hub session.
    async fn start(pc: Arc<Fake>, interval: Duration) -> FakeHub {
        let (kp, kh) = (Keys::generate(), Keys::generate());
        let (ip, ih) = tokio::io::duplex(1 << 16);
        let kh2 = kh.clone();
        let accepting = tokio::spawn(async move { accept(ih, &kh2, |_| true).await.unwrap() });
        let (rp, wp) = connect(ip, &kp, &kh.public).await.unwrap();
        let (_, rd, wr) = accepting.await.unwrap();
        let id = uuid::Uuid::new_v4();
        let (ctl, outbox) = SessionCtl::new();
        let task = tokio::spawn(run_hub_session(pc, id, rp, wp, ctl.clone(), outbox, interval));
        FakeHub { id, rd, wr, ctl, task }
    }

    async fn recv(&mut self) -> (Msg, Vec<u8>) {
        let f = tokio::time::timeout(Duration::from_secs(5), self.rd.recv()).await.expect("PC sent nothing").unwrap();
        (f.msg().unwrap(), f.payload)
    }

    /// The next frame from the PC, if one arrives within `wait`.
    async fn try_recv(&mut self, wait: Duration) -> Option<Msg> {
        match tokio::time::timeout(wait, self.rd.recv()).await {
            Ok(Ok(f)) => Some(f.msg().unwrap()),
            _ => None,
        }
    }

    async fn send(&mut self, m: Msg, payload: Vec<u8>) {
        self.wr.send(&Frame::new(&m, payload)).await.unwrap();
    }

    /// Takes the PC's hello and answers as a hub.
    async fn hello(&mut self, pc: uuid::Uuid) {
        match self.recv().await.0 {
            Msg::Hello { protocol, host_id, role, .. } => {
                assert_eq!((protocol, host_id, role), (SYNC_PROTOCOL, pc, Role::Pc));
            }
            other => panic!("expected hello, got {other:?}"),
        }
        let hello = Msg::Hello { protocol: SYNC_PROTOCOL, host_id: self.id, name: "Seek Hub".into(), role: Role::Hub };
        self.send(hello, vec![]).await;
    }

    /// Hello, the PC's `Devices`, an empty `Have` and the PC's upload up to its `Pull`; returns the pull's cursors.
    async fn through_first_pull(&mut self, pc: &Fake) -> Vec<(uuid::Uuid, Cursor)> {
        self.hello(pc.id).await;
        assert!(matches!(self.recv().await.0, Msg::Devices { .. }));
        self.send(Msg::Have { cursors: vec![] }, vec![]).await;
        loop {
            match self.recv().await.0 {
                Msg::Full { .. } | Msg::Delta { .. } => {}
                Msg::Pull { cursors } => return cursors,
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    /// Nudges the PC with *Sync now* until it pulls; returns the pull's cursors. A nudge that races ahead of the
    /// `PullDone` (or `Refused`) just sent is skipped, as during any pull in flight.
    async fn nudge_until_pull(&mut self) -> Vec<(uuid::Uuid, Cursor)> {
        for _ in 0..20 {
            self.ctl.sync_now.notify_one();
            match self.try_recv(Duration::from_millis(100)).await {
                None => {}
                Some(Msg::Pull { cursors }) => return cursors,
                Some(other) => panic!("expected pull, got {other:?}"),
            }
        }
        panic!("the PC never pulled again");
    }

    async fn stop(self) {
        self.ctl.stop.notify_one();
        let _ = self.task.await;
    }
}

const LONG: Duration = Duration::from_secs(3600);

fn pc_b() -> CatalogPc {
    CatalogPc {
        host_id: uuid::Uuid::new_v4(),
        name: "PC B".into(),
        user_sid: "S-1-5-21-1".into(),
        user_name: "me".into(),
        profiles: vec![("S-1-5-21-1".into(), "C:\\Users\\me".into())],
        last_seen_ms: 1,
    }
}

#[tokio::test]
async fn uploads_own_then_pulls() {
    let pc = Fake::new(1);
    let mut hub = FakeHub::start(pc.clone(), LONG).await;
    hub.hello(pc.id).await;
    match hub.recv().await.0 {
        Msg::Devices { devices, .. } => assert_eq!(devices, pc.own),
        other => panic!("expected devices, got {other:?}"),
    }
    hub.send(Msg::Have { cursors: vec![] }, vec![]).await;
    match hub.recv().await {
        (Msg::Full { device, cursor: c }, payload) => {
            assert_eq!((device, c, payload), (pc.own[0].id, cursor(1), vec![1, 2, 3]));
        }
        other => panic!("expected full, got {other:?}"),
    }
    match hub.recv().await.0 {
        Msg::Pull { cursors } => assert_eq!(cursors, pc.held),
        other => panic!("expected pull, got {other:?}"),
    }
    hub.stop().await;
}

#[tokio::test]
async fn applies_catalog_and_copies() {
    let pc = Fake::new(0);
    let mut hub = FakeHub::start(pc.clone(), LONG).await;
    hub.through_first_pull(&pc).await;
    let b = pc_b();
    let dev = device(uuid::Uuid::new_v4(), b.host_id);
    hub.send(Msg::Catalog { pcs: vec![b.clone()], devices: vec![dev.clone()] }, vec![]).await;
    hub.send(Msg::Full { device: dev.id, cursor: cursor(5) }, vec![9, 9]).await;
    hub.send(Msg::PullDone, vec![]).await;
    match hub.recv().await.0 {
        Msg::Ack { device, cursor: c } => assert_eq!((device, c), (dev.id, cursor(5))),
        other => panic!("expected ack, got {other:?}"),
    }
    assert_eq!(*pc.catalogs.lock().unwrap(), vec![(hub.id, vec![b.clone()], vec![dev.clone()])]);
    assert_eq!(*pc.fulls.lock().unwrap(), vec![(b.host_id, dev.id, cursor(5))]);
    hub.stop().await;
}

#[tokio::test]
async fn ignores_data_for_own_device() {
    let pc = Fake::new(1);
    let mut hub = FakeHub::start(pc.clone(), LONG).await;
    hub.through_first_pull(&pc).await;
    let b = pc_b();
    let own = pc.own[0].clone();
    let stranger = uuid::Uuid::new_v4();
    let theirs = device(uuid::Uuid::new_v4(), b.host_id);
    // the catalog wrongly lists this PC, its own device under its own id, and the same device as B's
    let mut me = pc_b();
    me.host_id = pc.id;
    let mut claimed = own.clone();
    claimed.indexed_by_host = b.host_id;
    let catalog = Msg::Catalog { pcs: vec![b.clone(), me], devices: vec![own.clone(), claimed, theirs.clone()] };
    hub.send(catalog, vec![]).await;
    hub.send(Msg::Full { device: own.id, cursor: cursor(5) }, vec![9]).await;
    hub.send(Msg::Full { device: stranger, cursor: cursor(5) }, vec![9]).await;
    hub.send(Msg::PullDone, vec![]).await;
    assert_eq!(hub.try_recv(Duration::from_millis(300)).await, None, "nothing is acknowledged");
    assert!(pc.fulls.lock().unwrap().is_empty());
    assert_eq!(*pc.catalogs.lock().unwrap(), vec![(hub.id, vec![b], vec![theirs])], "the host never sees this PC");
    hub.stop().await;
}

#[tokio::test]
async fn failed_apply_requests_full_on_next_pull() {
    let pc = Fake::new(0);
    let mut hub = FakeHub::start(pc.clone(), LONG).await;
    hub.through_first_pull(&pc).await;
    let b = pc_b();
    let dev = device(uuid::Uuid::new_v4(), b.host_id);
    let catalog = Msg::Catalog { pcs: vec![b.clone()], devices: vec![dev.clone()] };
    pc.fail_next_apply.store(true, Ordering::SeqCst);
    hub.send(catalog.clone(), vec![]).await;
    hub.send(Msg::Full { device: dev.id, cursor: cursor(5) }, vec![9]).await;
    hub.send(Msg::PullDone, vec![]).await;
    assert_eq!(hub.try_recv(Duration::from_millis(300)).await, None, "a failed copy gets no answer");
    let mut want = pc.held.clone();
    want.push((dev.id, Cursor::none()));
    assert_eq!(hub.nudge_until_pull().await, want, "the next pull asks for a full copy");
    // the full copy applies: acknowledged, and the pull after it reports the held cursors again
    hub.send(catalog, vec![]).await;
    hub.send(Msg::Full { device: dev.id, cursor: cursor(6) }, vec![9]).await;
    hub.send(Msg::PullDone, vec![]).await;
    assert_eq!(hub.recv().await.0, Msg::Ack { device: dev.id, cursor: cursor(6) });
    assert_eq!(hub.nudge_until_pull().await, pc.held);
    hub.stop().await;
}

#[tokio::test]
async fn refused_pull_does_not_stall_pulling() {
    let pc = Fake::new(0);
    let mut hub = FakeHub::start(pc.clone(), LONG).await;
    hub.through_first_pull(&pc).await;
    hub.send(Msg::Refused { reason: Refusal::NotSupported }, vec![]).await;
    assert_eq!(hub.nudge_until_pull().await, pc.held);
    assert_eq!(*pc.refused.lock().unwrap(), vec![Refusal::NotSupported]);
    hub.stop().await;
}

#[tokio::test]
async fn no_second_pull_in_flight() {
    let pc = Fake::new(0);
    let interval = Duration::from_secs(60);
    let mut hub = FakeHub::start(pc.clone(), interval).await;
    hub.through_first_pull(&pc).await;
    // two ticks and an index change while the pull is unanswered; the clock runs again before waiting on the
    // PC, whose blocking host calls would otherwise let a paused clock jump past any timeout
    tokio::time::pause();
    tokio::time::sleep(interval * 2 + Duration::from_secs(1)).await;
    hub.ctl.changed.notify_one();
    tokio::time::resume();
    assert_eq!(hub.try_recv(Duration::from_millis(500)).await, None, "no second pull before PullDone");
    hub.send(Msg::Catalog { pcs: vec![], devices: vec![] }, vec![]).await;
    hub.send(Msg::PullDone, vec![]).await;
    hub.nudge_until_pull().await;
    hub.stop().await;
}

#[tokio::test]
async fn refusal_reaches_the_host() {
    let pc = Fake::new(0);
    let mut hub = FakeHub::start(pc.clone(), LONG).await;
    hub.through_first_pull(&pc).await;
    hub.send(Msg::Refused { reason: Refusal::QuotaFull }, vec![]).await;
    hub.send(Msg::Unlinked, vec![]).await;
    assert!(hub.task.await.unwrap().is_ok());
    assert_eq!(*pc.refused.lock().unwrap(), vec![Refusal::QuotaFull]);
}

#[tokio::test]
async fn unlinked_reply_ends_session() {
    let pc = Fake::new(1);
    let mut hub = FakeHub::start(pc.clone(), LONG).await;
    // a hub that unlinked this PC answers its hello with `Unlinked` alone, no hello of its own
    assert!(matches!(hub.recv().await.0, Msg::Hello { .. }));
    hub.send(Msg::Unlinked, vec![]).await;
    assert!(hub.task.await.unwrap().is_ok());
    assert!(pc.unlinked.load(Ordering::SeqCst));
}

#[tokio::test]
async fn rejects_non_hub_peer() {
    let pc = Fake::new(1);
    let mut hub = FakeHub::start(pc.clone(), LONG).await;
    let _ = hub.recv().await;
    let hello = Msg::Hello { protocol: SYNC_PROTOCOL, host_id: hub.id, name: "PC B".into(), role: Role::Pc };
    hub.send(hello, vec![]).await;
    assert!(matches!(hub.task.await.unwrap(), Err(Error::Protocol(_))));
    assert!(!pc.unlinked.load(Ordering::SeqCst));
}

#[tokio::test]
async fn refused_devices_turns_uploads_off_but_still_pulls() {
    let pc = Fake::new(1);
    let mut hub = FakeHub::start(pc.clone(), LONG).await;
    hub.hello(pc.id).await;
    assert!(matches!(hub.recv().await.0, Msg::Devices { .. }));
    // the hub will not take this PC's devices, so no `Have` follows: downloads must work anyway
    hub.send(Msg::Refused { reason: Refusal::TooManyDevices }, vec![]).await;
    match hub.recv().await.0 {
        Msg::Pull { cursors } => assert_eq!(cursors, pc.held),
        other => panic!("expected pull, got {other:?}"),
    }
    hub.send(Msg::Catalog { pcs: vec![], devices: vec![] }, vec![]).await;
    hub.send(Msg::PullDone, vec![]).await;
    // later polls pull and upload nothing
    assert_eq!(hub.nudge_until_pull().await, pc.held);
    assert_eq!(*pc.refused.lock().unwrap(), vec![Refusal::TooManyDevices]);
    hub.stop().await;
}

#[tokio::test]
async fn refused_upload_keeps_uploads_on() {
    let pc = Fake::new(1);
    let mut hub = FakeHub::start(pc.clone(), LONG).await;
    hub.through_first_pull(&pc).await;
    hub.send(Msg::PullDone, vec![]).await;
    // one upload went over quota; that says nothing about the next one
    hub.send(Msg::Refused { reason: Refusal::QuotaFull }, vec![]).await;
    tokio::time::sleep(Duration::from_millis(200)).await; // let the PC handle both before it is nudged
    pc.full_always.store(true, Ordering::SeqCst);
    hub.ctl.sync_now.notify_one();
    match hub.recv().await.0 {
        Msg::Full { device, .. } => assert_eq!(device, pc.own[0].id),
        other => panic!("expected an upload, got {other:?}"),
    }
    assert!(matches!(hub.recv().await.0, Msg::Pull { .. }));
    assert_eq!(*pc.refused.lock().unwrap(), vec![Refusal::QuotaFull]);
    hub.stop().await;
}

#[tokio::test]
async fn devices_refusal_during_pull_is_a_devices_refusal() {
    let pc = Fake::new(0);
    let mut hub = FakeHub::start(pc.clone(), LONG).await;
    hub.through_first_pull(&pc).await; // a pull is now in flight
    pc.extra.lock().unwrap().push(device(uuid::Uuid::new_v4(), pc.id));
    hub.ctl.sync_now.notify_one();
    assert!(matches!(hub.recv().await.0, Msg::Devices { .. }));
    let _ = hub.try_recv(Duration::from_millis(200)).await; // the upload that followed the list
    hub.send(Msg::Refused { reason: Refusal::TooManyDevices }, vec![]).await;
    // the pull is still unanswered, so it is not pulled again; the refusal did not reset it
    hub.ctl.sync_now.notify_one();
    assert_eq!(hub.try_recv(Duration::from_millis(300)).await, None);
    hub.send(Msg::PullDone, vec![]).await;
    assert_eq!(hub.nudge_until_pull().await, pc.held);
    assert_eq!(*pc.refused.lock().unwrap(), vec![Refusal::TooManyDevices]);
    hub.stop().await;
}
