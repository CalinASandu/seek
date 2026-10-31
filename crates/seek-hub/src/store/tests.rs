use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use seek_core::{
    decode_index, encode_index, synth_index, Change, Device, DeviceIdentity, DeviceIndex, DeviceKind, DeviceState,
    IndexHeader, RawEntry,
};
use seek_sync::session::{decode_delta, encode_delta};
use seek_sync::{Cursor, Keys, Refusal};
use uuid::Uuid;

use super::*;
use crate::crypt::HubKey;

/// Fails reads (`write == false`) or writes of paths ending in `suffix` while `flag` is set.
fn fail_when(flag: &Arc<AtomicBool>, write: bool, suffix: &'static str) -> Fault {
    let flag = flag.clone();
    Box::new(move |w: bool, p: &Path| {
        (w == write && flag.load(Ordering::SeqCst) && p.to_string_lossy().ends_with(suffix))
            .then(|| io::Error::other("injected disk error"))
    })
}

/// A valid delta payload of at least `n` bytes (random names, so zstd cannot shrink it much).
fn delta_of_size(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut changes = Vec::new();
    loop {
        let p = encode_delta(&changes);
        if p.len() >= n {
            return p;
        }
        let name: String = (0..16)
            .map(|_| {
                x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                char::from(b'a' + ((x >> 59) as u8) % 26)
            })
            .collect();
        changes.push(Change::Upsert(entry(1000 + changes.len() as u64, 5, &name)));
    }
}

struct Env {
    _dir: tempfile::TempDir,
    cfg: HubConfig,
    key: HubKey,
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
    Env { _dir: dir, cfg, key }
}

impl Env {
    fn open(&self) -> Store {
        Store::open(&self.cfg, self.key.clone()).unwrap()
    }
}

fn pc(n: u128) -> (PcRecord, [u8; 32]) {
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
    (rec, keys.public)
}

fn device(id: Uuid, host: Uuid) -> Device {
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

fn full_bytes(device: Uuid, host: Uuid, ix: &DeviceIndex) -> Vec<u8> {
    let hdr = IndexHeader {
        format_version: 0,
        device_id: device,
        host_id: host,
        checkpoint: None,
        saved_at: 0,
        rows: 0,
        sync_cursor: None,
    };
    encode_index(&hdr, &mut ix.clone()).unwrap()
}

fn cur(g: Uuid, seq: u64) -> Cursor {
    Cursor { generation: g, seq }
}

fn entry(id: u64, parent: u64, name: &str) -> RawEntry {
    RawEntry { id, parent_id: parent, name: name.into(), size: 1, modified: 0, created: 0, attrs: 0 }
}

fn live_names(ix: &DeviceIndex) -> Vec<String> {
    let mut v: Vec<String> =
        (0..ix.len() as u32).filter(|&r| ix.is_alive(r)).map(|r| ix.name(r).to_string()).collect();
    v.sort();
    v
}

/// Store with PC 1 owning device 10 (and PC 2 linked, no drives).
fn one_pc_one_drive(e: &Env) -> (Store, PcRecord, Uuid) {
    let s = e.open();
    let (a, _) = pc(1);
    let (b, _) = pc(2);
    s.add_pc(a.clone()).unwrap();
    s.add_pc(b).unwrap();
    let d = Uuid::from_u128(10);
    s.set_pc_devices(a.host_id, vec![device(d, a.host_id)], vec![]).unwrap();
    (s, a, d)
}

fn concat_changes(served: &Served) -> Vec<Change> {
    match served {
        Served::Delta { payload, .. } => decode_delta(payload).unwrap(),
        Served::Full { .. } => panic!("expected a delta"),
    }
}

#[test]
fn ownership_first_claim_wins() {
    let e = env();
    let s = e.open();
    let (a, _) = pc(1);
    let (b, _) = pc(2);
    s.add_pc(a.clone()).unwrap();
    s.add_pc(b.clone()).unwrap();
    let d = Uuid::from_u128(10);
    s.set_pc_devices(a.host_id, vec![device(d, a.host_id)], vec![]).unwrap();
    assert_eq!(
        s.set_pc_devices(b.host_id, vec![device(d, b.host_id)], vec![]),
        Err(Refusal::NotOwner { device: d })
    );
    let g = Uuid::from_u128(77);
    assert_eq!(s.put_full(b.host_id, d, cur(g, 0), crate::store::fake_copy(&[1, 2, 3])), Err(Refusal::NotOwner { device: d }));
    assert_eq!(
        s.put_delta(b.host_id, d, cur(g, 0), cur(g, 1), vec![1]),
        Err(PutError::Refused(Refusal::NotOwner { device: d }))
    );
    s.put_full(a.host_id, d, cur(g, 0), crate::store::fake_copy(&[1, 2, 3])).unwrap();
    // Once A drops the device, B may claim it.
    s.set_pc_devices(a.host_id, vec![], vec![]).unwrap();
    s.set_pc_devices(b.host_id, vec![device(d, b.host_id)], vec![]).unwrap();
    assert_eq!(s.cursor(d), None, "A's copy went with A's claim");
}

#[test]
fn quota_full() {
    let mut e = env();
    e.cfg.pc_quota_bytes = 1024;
    let (s, a, d) = one_pc_one_drive(&e);
    let g = Uuid::from_u128(77);
    assert_eq!(s.put_full(a.host_id, d, cur(g, 0), crate::store::fake_copy(&vec![0u8; 2048])), Err(Refusal::QuotaFull));
    assert_eq!(s.cursor(d), None);
    s.put_full(a.host_id, d, cur(g, 0), crate::store::fake_copy(&[0u8; 200])).unwrap();
    // Deltas count too.
    assert_eq!(
        s.put_delta(a.host_id, d, cur(g, 0), cur(g, 1), delta_of_size(900, 1)),
        Err(PutError::Refused(Refusal::QuotaFull))
    );
    assert_eq!(s.cursor(d), Some(cur(g, 0)));
}

#[test]
fn too_many_devices() {
    let mut e = env();
    e.cfg.pc_max_devices = 2;
    let s = e.open();
    let (a, _) = pc(1);
    s.add_pc(a.clone()).unwrap();
    let devs: Vec<Device> = (10..13).map(|i| device(Uuid::from_u128(i), a.host_id)).collect();
    assert_eq!(s.set_pc_devices(a.host_id, devs.clone(), vec![]), Err(Refusal::TooManyDevices));
    s.set_pc_devices(a.host_id, devs[..2].to_vec(), vec![]).unwrap();
    assert_eq!(s.stats().devices, 2);
}

#[test]
fn serve_delta_full_or_nothing() {
    let e = env();
    let (s, a, d) = one_pc_one_drive(&e);
    let g = Uuid::from_u128(77);
    assert!(s.serve(d, None).is_empty(), "nothing stored yet");
    s.put_full(a.host_id, d, cur(g, 0), crate::store::fake_copy(b"full-0")).unwrap();
    // A full copy without a journal: just the full.
    assert_eq!(s.serve(d, None), vec![Served::Full { cursor: cur(g, 0), bytes: crate::store::fake_copy(b"full-0") }]);

    let c1 = vec![Change::Delete { id: 1 }, Change::Delete { id: 2 }];
    let c2 = vec![Change::Upsert(entry(9, 5, "nine")), Change::Delete { id: 3 }, Change::Delete { id: 4 }];
    s.put_delta(a.host_id, d, cur(g, 0), cur(g, 2), encode_delta(&c1)).unwrap();
    s.put_delta(a.host_id, d, cur(g, 2), cur(g, 5), encode_delta(&c2)).unwrap();
    assert_eq!(s.cursor(d), Some(cur(g, 5)));

    // 1. up to date → nothing
    assert!(s.serve(d, Some(cur(g, 5))).is_empty());
    // 2. in the journal's range → one delta of the batches after it
    let out = s.serve(d, Some(cur(g, 2)));
    assert_eq!(out.len(), 1);
    assert!(matches!(&out[0], Served::Delta { from, to, .. } if *from == cur(g, 2) && *to == cur(g, 5)));
    assert_eq!(concat_changes(&out[0]), c2);
    let out = s.serve(d, Some(cur(g, 0)));
    assert_eq!(out.len(), 1);
    assert_eq!(concat_changes(&out[0]), [c1.clone(), c2.clone()].concat());
    // 3. unknown / other generation / no cursor → full then delta from the full to the newest
    for have in [None, Some(cur(Uuid::from_u128(5), 3)), Some(cur(g, 9))] {
        let out = s.serve(d, have);
        assert_eq!(out.len(), 2, "{have:?}");
        assert_eq!(out[0], Served::Full { cursor: cur(g, 0), bytes: crate::store::fake_copy(b"full-0") });
        assert!(matches!(&out[1], Served::Delta { from, to, .. } if *from == cur(g, 0) && *to == cur(g, 5)));
        assert_eq!(concat_changes(&out[1]), [c1.clone(), c2.clone()].concat());
    }
}

#[test]
fn delta_mismatch_rejected() {
    let e = env();
    let (s, a, d) = one_pc_one_drive(&e);
    let g = Uuid::from_u128(77);
    let p = encode_delta(&[Change::Delete { id: 1 }]);
    assert_eq!(s.put_delta(a.host_id, d, cur(g, 0), cur(g, 1), p.clone()), Err(PutError::Mismatch), "no full yet");
    s.put_full(a.host_id, d, cur(g, 0), crate::store::fake_copy(b"f")).unwrap();
    assert_eq!(s.put_delta(a.host_id, d, cur(g, 3), cur(g, 4), p.clone()), Err(PutError::Mismatch));
    assert_eq!(
        s.put_delta(a.host_id, d, cur(Uuid::from_u128(5), 0), cur(Uuid::from_u128(5), 1), p.clone()),
        Err(PutError::Mismatch)
    );
    s.put_delta(a.host_id, d, cur(g, 0), cur(g, 1), p).unwrap();
    assert_eq!(s.cursor(d), Some(cur(g, 1)));
}

#[test]
fn compaction_equals_applying_journal() {
    let e = env();
    let (s, a, d) = one_pc_one_drive(&e);
    let g = Uuid::from_u128(77);
    let base = synth_index(300, 7);
    s.put_full(a.host_id, d, cur(g, 0), full_bytes(d, a.host_id, &base)).unwrap();

    let root = base.root_id();
    let batches = [
        vec![Change::Upsert(entry(10_000, root, "added one.txt")), Change::Delete { id: base.id(5) }],
        vec![Change::Upsert(entry(10_001, root, "added two.txt")), Change::Delete { id: base.id(9) }],
        vec![Change::Delete { id: 10_000 }, Change::Upsert(entry(10_002, root, "added three.txt"))],
    ];
    let mut expected = base.clone();
    for (seq, b) in (0u64..).zip(&batches) {
        s.put_delta(a.host_id, d, cur(g, seq), cur(g, seq + 1), encode_delta(b)).unwrap();
        expected.apply(b);
    }
    s.compact(d).unwrap();
    assert!(!s.compact_due().contains(&d));

    let out = s.serve(d, None);
    assert_eq!(out.len(), 1, "journal folded into the full copy");
    let Served::Full { cursor, bytes } = &out[0] else { panic!("expected full") };
    assert_eq!(*cursor, cur(g, 3));
    let (_, got) = decode_index(bytes).unwrap();
    assert_eq!(got.live_count(), expected.live_count());
    assert_eq!(live_names(&got), live_names(&expected));
    assert!(s.serve(d, Some(cur(g, 3))).is_empty());
    // New deltas chain from the compacted cursor.
    s.put_delta(a.host_id, d, cur(g, 3), cur(g, 4), encode_delta(&[Change::Delete { id: 10_001 }])).unwrap();
}

#[test]
fn compaction_interrupted_before_journal_rewrite() {
    let e = env();
    let (s, a, d) = one_pc_one_drive(&e);
    let g = Uuid::from_u128(77);
    let base = synth_index(50, 3);
    s.put_full(a.host_id, d, cur(g, 0), full_bytes(d, a.host_id, &base)).unwrap();
    for seq in 0..2 {
        let c = [Change::Upsert(entry(20_000 + seq, base.root_id(), "x"))];
        s.put_delta(a.host_id, d, cur(g, seq), cur(g, seq + 1), encode_delta(&c)).unwrap();
    }
    let journal = e.cfg.data_dir.join("copies").join(format!("{d}.journal"));
    let old = std::fs::read(&journal).unwrap();
    s.compact(d).unwrap();
    drop(s);
    // As if the hub died after writing the new full copy but before removing the journal.
    std::fs::write(&journal, old).unwrap();
    let s = e.open();
    assert_eq!(s.cursor(d), Some(cur(g, 2)));
    let out = s.serve(d, None);
    assert_eq!(out.len(), 1, "stale batches before the full copy are skipped");
    assert!(!journal.exists(), "the stale journal is cut back");
}

#[test]
fn compaction_due_threshold() {
    let e = env();
    let (s, a, d) = one_pc_one_drive(&e);
    let g = Uuid::from_u128(77);
    s.put_full(a.host_id, d, cur(g, 0), crate::store::fake_copy(&vec![0u8; 4000])).unwrap();
    s.put_delta(a.host_id, d, cur(g, 0), cur(g, 1), delta_of_size(100, 2)).unwrap();
    assert!(s.compact_due().is_empty(), "journal well under 25 %");
    s.put_delta(a.host_id, d, cur(g, 1), cur(g, 2), delta_of_size(1000, 3)).unwrap();
    assert_eq!(s.compact_due(), vec![d]);
}

#[test]
fn restart_keeps_everything() {
    let e = env();
    let (s, a, d) = one_pc_one_drive(&e);
    let (gone, gone_key) = pc(3);
    s.add_pc(gone.clone()).unwrap();
    assert!(s.unlink_pc(gone.host_id).unwrap());
    let g = Uuid::from_u128(77);
    s.put_full(a.host_id, d, cur(g, 0), crate::store::fake_copy(b"full")).unwrap();
    s.put_delta(a.host_id, d, cur(g, 0), cur(g, 1), encode_delta(&[Change::Delete { id: 1 }])).unwrap();
    s.touch_pc(a.host_id, 123_456);
    let (id, public, pcs, served, cat, stats) = (
        s.hub_id(),
        s.noise_keys().public,
        s.pcs(),
        s.serve(d, None),
        s.catalog_for(Uuid::from_u128(2)),
        s.stats(),
    );
    drop(s);

    let s = e.open();
    assert_eq!(s.hub_id(), id);
    assert_eq!(s.name(), "Test Hub");
    assert_eq!(s.noise_keys().public, public);
    assert_eq!(s.pcs(), pcs);
    assert_eq!(s.cursor(d), Some(cur(g, 1)));
    assert_eq!(s.serve(d, None), served);
    assert_eq!(s.catalog_for(Uuid::from_u128(2)), cat);
    assert_eq!(s.stats(), stats);
    assert!(s.is_tombstoned(&gone_key));
}

#[test]
fn wrong_key_on_reopen_is_unreadable() {
    let e = env();
    drop(one_pc_one_drive(&e));
    let other = tempfile::tempdir().unwrap();
    let wrong = HubKey::create(other.path()).unwrap();
    match Store::open(&e.cfg, wrong) {
        Err(StoreError::Unreadable(p)) => assert_eq!(p.file_name().unwrap(), "hub.json"),
        Err(other) => panic!("wrong error {other}"),
        Ok(_) => panic!("opened with the wrong key"),
    }
}

#[test]
fn unlink_deletes_copies_and_tombstones() {
    let e = env();
    let (s, a, d) = one_pc_one_drive(&e);
    let a_key = seek_sync::peers::key_from_b64(&a.public_key).unwrap();
    let g = Uuid::from_u128(77);
    s.put_full(a.host_id, d, cur(g, 0), crate::store::fake_copy(b"full")).unwrap();
    s.put_delta(a.host_id, d, cur(g, 0), cur(g, 1), delta_of_size(10, 4)).unwrap();
    let copies = e.cfg.data_dir.join("copies");
    assert_eq!(std::fs::read_dir(&copies).unwrap().count(), 2);

    assert!(s.unlink_pc(a.host_id).unwrap());
    assert!(!s.unlink_pc(a.host_id).unwrap());
    assert_eq!(s.cursor(d), None);
    assert!(s.serve(d, None).is_empty());
    assert_eq!(std::fs::read_dir(&copies).unwrap().count(), 0);
    assert!(s.is_tombstoned(&a_key));
    assert!(s.pc_by_key(&a_key).is_none());
    assert_eq!(s.pcs().len(), 1);
    let st = s.stats();
    assert_eq!((st.pcs, st.devices, st.stored_bytes), (1, 0, 0));
}

#[test]
fn relink_clears_tombstone() {
    let e = env();
    let (s, a, _) = one_pc_one_drive(&e);
    let a_key = seek_sync::peers::key_from_b64(&a.public_key).unwrap();
    s.unlink_pc(a.host_id).unwrap();
    assert!(s.is_tombstoned(&a_key));
    s.add_pc(a.clone()).unwrap();
    assert!(!s.is_tombstoned(&a_key));
    assert_eq!(s.pc_by_key(&a_key), Some(a));
}

#[test]
fn relink_same_host_new_key_keeps_drives() {
    let e = env();
    let (s, a, d) = one_pc_one_drive(&e);
    let old_key = seek_sync::peers::key_from_b64(&a.public_key).unwrap();
    let g = Uuid::from_u128(77);
    s.put_full(a.host_id, d, cur(g, 0), crate::store::fake_copy(b"full")).unwrap();

    let (mut again, new_key) = pc(9);
    again.host_id = a.host_id;
    again.name = a.name.clone();
    s.add_pc(again.clone()).unwrap();
    assert!(s.pc_by_key(&old_key).is_none());
    assert_eq!(s.pc_by_key(&new_key), Some(again));
    assert_eq!(s.pcs().len(), 2);
    assert_eq!(s.cursor(d), Some(cur(g, 0)));
    let (_, devices) = s.catalog_for(Uuid::from_u128(2));
    assert_eq!(devices.iter().map(|x| x.id).collect::<Vec<_>>(), vec![d]);
}

#[test]
fn partial_full_upload_keeps_old_copy() {
    let e = env();
    let (s, a, d) = one_pc_one_drive(&e);
    let g = Uuid::from_u128(77);
    s.put_full(a.host_id, d, cur(g, 0), crate::store::fake_copy(b"old full")).unwrap();
    let flag = Arc::new(AtomicBool::new(true));
    s.set_fault(Some(fail_when(&flag, true, ".full")));
    let g2 = Uuid::from_u128(78);
    assert_eq!(s.put_full(a.host_id, d, cur(g2, 0), crate::store::fake_copy(b"new full")), Err(Refusal::HubFull));
    assert_eq!(s.cursor(d), Some(cur(g, 0)));
    assert_eq!(s.serve(d, None), vec![Served::Full { cursor: cur(g, 0), bytes: crate::store::fake_copy(b"old full") }]);
    // Writing works again once the disk does.
    flag.store(false, Ordering::SeqCst);
    s.put_full(a.host_id, d, cur(g2, 0), crate::store::fake_copy(b"new full")).unwrap();
    assert_eq!(s.cursor(d), Some(cur(g2, 0)));
}

#[test]
fn transient_read_error_keeps_copy() {
    let e = env();
    let (s, a, d) = one_pc_one_drive(&e);
    let g = Uuid::from_u128(77);
    s.put_full(a.host_id, d, cur(g, 0), crate::store::fake_copy(b"full")).unwrap();
    s.put_delta(a.host_id, d, cur(g, 0), cur(g, 1), delta_of_size(10, 5)).unwrap();
    let want = s.serve(d, None);
    assert_eq!(want.len(), 2);
    drop(s);

    // Not loaded yet: the full copy cannot be read.
    let s = e.open();
    let flag = Arc::new(AtomicBool::new(true));
    s.set_fault(Some(fail_when(&flag, false, ".full")));
    assert_eq!(s.cursor(d), None);
    assert!(s.serve(d, None).is_empty());
    let copies = e.cfg.data_dir.join("copies");
    assert!(copies.join(format!("{d}.full")).exists());
    assert!(copies.join(format!("{d}.journal")).exists());
    flag.store(false, Ordering::SeqCst);
    assert_eq!(s.cursor(d), Some(cur(g, 1)));
    assert_eq!(s.serve(d, None), want);

    // Loaded: the journal cannot be read.
    s.set_fault(Some(fail_when(&flag, false, ".journal")));
    flag.store(true, Ordering::SeqCst);
    assert!(s.serve(d, None).is_empty());
    assert_eq!(
        s.put_delta(a.host_id, d, cur(g, 1), cur(g, 2), delta_of_size(10, 6)),
        Err(PutError::Refused(Refusal::HubFull))
    );
    assert!(matches!(s.compact(d), Err(StoreError::Io(_))));
    flag.store(false, Ordering::SeqCst);
    assert_eq!(s.serve(d, None), want);
}

#[test]
fn tampered_copy_is_dropped() {
    let e = env();
    let (s, a, d) = one_pc_one_drive(&e);
    let g = Uuid::from_u128(77);
    s.put_full(a.host_id, d, cur(g, 0), crate::store::fake_copy(b"full")).unwrap();
    s.put_delta(a.host_id, d, cur(g, 0), cur(g, 1), delta_of_size(10, 5)).unwrap();
    drop(s);
    let copies = e.cfg.data_dir.join("copies");
    let full = copies.join(format!("{d}.full"));
    let mut b = std::fs::read(&full).unwrap();
    let last = b.len() - 1;
    b[last] ^= 1;
    std::fs::write(&full, b).unwrap();

    let s = e.open();
    assert_eq!(s.cursor(d), None);
    assert!(s.serve(d, None).is_empty());
    assert!(!full.exists());
    assert!(!copies.join(format!("{d}.journal")).exists());
}

#[test]
fn unlink_save_failure_changes_nothing() {
    let e = env();
    let (s, a, d) = one_pc_one_drive(&e);
    let a_key = seek_sync::peers::key_from_b64(&a.public_key).unwrap();
    let g = Uuid::from_u128(77);
    s.put_full(a.host_id, d, cur(g, 0), crate::store::fake_copy(b"full")).unwrap();
    let flag = Arc::new(AtomicBool::new(true));
    s.set_fault(Some(fail_when(&flag, true, "pcs.json")));
    assert!(s.unlink_pc(a.host_id).is_err());
    assert!(s.pc_by_key(&a_key).is_some());
    assert!(!s.is_tombstoned(&a_key));
    assert_eq!(s.cursor(d), Some(cur(g, 0)));
    drop(s);

    let s = e.open();
    assert!(s.pc_by_key(&a_key).is_some(), "still linked after a restart");
    assert_eq!(s.cursor(d), Some(cur(g, 0)));
    assert!(s.unlink_pc(a.host_id).unwrap());
    assert!(s.is_tombstoned(&a_key));
}

#[test]
fn device_save_failure_changes_nothing() {
    let e = env();
    let (s, a, d) = one_pc_one_drive(&e);
    let g = Uuid::from_u128(77);
    s.put_full(a.host_id, d, cur(g, 0), crate::store::fake_copy(b"full")).unwrap();
    let flag = Arc::new(AtomicBool::new(true));
    s.set_fault(Some(fail_when(&flag, true, "devices.json")));
    assert_eq!(s.set_pc_devices(a.host_id, vec![], vec![]), Err(Refusal::HubFull));
    assert_eq!(s.cursor(d), Some(cur(g, 0)));
    assert_eq!(s.catalog_for(Uuid::from_u128(2)).1.len(), 1);
    flag.store(false, Ordering::SeqCst);
    s.set_pc_devices(a.host_id, vec![], vec![]).unwrap();
    assert_eq!(s.cursor(d), None);
    assert!(s.catalog_for(Uuid::from_u128(2)).1.is_empty());
}

#[test]
fn undecodable_delta_is_mismatch() {
    let e = env();
    let (s, a, d) = one_pc_one_drive(&e);
    let g = Uuid::from_u128(77);
    s.put_full(a.host_id, d, cur(g, 0), crate::store::fake_copy(b"full")).unwrap();
    assert_eq!(s.put_delta(a.host_id, d, cur(g, 0), cur(g, 1), vec![1, 2, 3]), Err(PutError::Mismatch));
    assert_eq!(s.cursor(d), Some(cur(g, 0)));
    assert_eq!(s.serve(d, None).len(), 1);
}

#[test]
fn same_key_new_host_replaces_old_record() {
    let e = env();
    let (s, a, _) = one_pc_one_drive(&e);
    let key = seek_sync::peers::key_from_b64(&a.public_key).unwrap();
    let mut moved = a.clone();
    moved.host_id = Uuid::from_u128(5);
    s.add_pc(moved).unwrap();
    assert_eq!(s.pc_by_key(&key).unwrap().host_id, Uuid::from_u128(5));
    assert_eq!(s.pcs().iter().filter(|p| p.public_key == a.public_key).count(), 1);
    assert!(s.pcs().iter().all(|p| p.host_id != a.host_id));
    assert!(s.catalog_for(Uuid::from_u128(2)).1.is_empty(), "the old record's drives went with it");
}

#[test]
fn device_indexed_by_other_host_refused() {
    let e = env();
    let s = e.open();
    let (a, _) = pc(1);
    s.add_pc(a.clone()).unwrap();
    let d = Uuid::from_u128(10);
    assert_eq!(
        s.set_pc_devices(a.host_id, vec![device(d, Uuid::from_u128(2))], vec![]),
        Err(Refusal::NotOwner { device: d })
    );
    assert_eq!(s.stats().devices, 0);
}

#[test]
fn full_over_journal_replaces_both() {
    let mut e = env();
    e.cfg.pc_quota_bytes = 1024;
    let (s, a, d) = one_pc_one_drive(&e);
    let g = Uuid::from_u128(77);
    s.put_full(a.host_id, d, cur(g, 0), crate::store::fake_copy(&vec![1u8; 600])).unwrap();
    s.put_delta(a.host_id, d, cur(g, 0), cur(g, 1), delta_of_size(10, 7)).unwrap();
    // The old copy does not count against the new one.
    let g2 = Uuid::from_u128(78);
    s.put_full(a.host_id, d, cur(g2, 0), crate::store::fake_copy(&vec![2u8; 600])).unwrap();
    assert_eq!(s.serve(d, None), vec![Served::Full { cursor: cur(g2, 0), bytes: crate::store::fake_copy(&vec![2u8; 600]) }]);
    assert!(s.serve(d, Some(cur(g, 0))).len() == 1, "old deltas are gone");
    assert!(!e.cfg.data_dir.join("copies").join(format!("{d}.journal")).exists());
}

#[test]
fn catalog_excludes_requester() {
    let e = env();
    let s = e.open();
    let (a, _) = pc(1);
    let (b, _) = pc(2);
    s.add_pc(a.clone()).unwrap();
    s.add_pc(b.clone()).unwrap();
    let (da, db) = (Uuid::from_u128(10), Uuid::from_u128(20));
    s.set_pc_devices(a.host_id, vec![device(da, a.host_id)], vec![]).unwrap();
    let profiles = vec![("S-1-2".to_string(), "C:\\Users\\b".to_string())];
    s.set_pc_devices(b.host_id, vec![device(db, b.host_id)], profiles.clone()).unwrap();

    let (pcs, devices) = s.catalog_for(a.host_id);
    assert_eq!(pcs.len(), 1);
    assert_eq!(pcs[0].host_id, b.host_id);
    assert_eq!(pcs[0].profiles, profiles);
    assert_eq!(devices, vec![device(db, b.host_id)]);
    let (pcs, devices) = s.catalog_for(b.host_id);
    assert_eq!(pcs[0].host_id, a.host_id);
    assert_eq!(devices, vec![device(da, a.host_id)]);
}

#[test]
fn known_keys_follow_link_and_unlink() {
    let e = env();
    let s = e.open();
    let (a, ka) = pc(1);
    assert!(!s.is_known_key(&ka));
    s.add_pc(a.clone()).unwrap();
    assert!(s.is_known_key(&ka));
    s.unlink_pc(a.host_id).unwrap();
    assert!(s.is_known_key(&ka), "a tombstoned key is still answered");
    drop(s);
    assert!(e.open().is_known_key(&ka), "and after a restart");
}

#[test]
fn full_copy_that_is_not_an_index_is_refused() {
    let e = env();
    let (s, a, d) = one_pc_one_drive(&e);
    let g = Uuid::from_u128(77);
    assert_eq!(s.put_full(a.host_id, d, cur(g, 0), vec![0u8; 1000]), Err(Refusal::NotSupported));
    assert_eq!(s.cursor(d), None);
}

#[test]
fn pc_metadata_is_bounded() {
    let e = env();
    let (s, a, d) = one_pc_one_drive(&e);
    let ok = vec![device(d, a.host_id)];
    let profile = ("S-1".to_string(), "C:/Users/x".to_string());
    let many = vec![profile.clone(); 65];
    assert_eq!(s.set_pc_devices(a.host_id, ok.clone(), many), Err(Refusal::QuotaFull));
    let long = vec![("S".repeat(513), "C:".to_string())];
    assert_eq!(s.set_pc_devices(a.host_id, ok.clone(), long), Err(Refusal::QuotaFull));
    let mut fat = device(d, a.host_id);
    fat.display_name = "n".repeat(5000);
    assert_eq!(s.set_pc_devices(a.host_id, vec![fat], vec![profile.clone()]), Err(Refusal::QuotaFull));
    s.set_pc_devices(a.host_id, ok, vec![profile; 64]).unwrap();
}
