use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
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
