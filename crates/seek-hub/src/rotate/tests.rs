use seek_core::{Device, DeviceIdentity, DeviceKind, DeviceState};
use seek_sync::{Cursor, Keys};
use uuid::Uuid;

use super::*;
use crate::config::HubConfig;
use crate::store::{PcRecord, Served, Store};

struct Env {
    _dir: tempfile::TempDir,
    cfg: HubConfig,
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
    HubKey::create(&cfg.keys_dir).unwrap();
    Env { _dir: dir, cfg }
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

const PC: Uuid = Uuid::from_u128(1);
const A: Uuid = Uuid::from_u128(10);
const B: Uuid = Uuid::from_u128(11);

/// A store with one PC owning drives A and B, each with a stored copy; returns the hub id.
fn fill(e: &Env) -> Uuid {
    let s = Store::open(&e.cfg, HubKey::load(&e.cfg.keys_dir).unwrap()).unwrap();
    let rec = PcRecord {
        host_id: PC,
        name: "Laptop".into(),
        public_key: seek_sync::peers::key_to_b64(&Keys::generate().public),
        user_sid: "S-1-5".into(),
        user_name: "me".into(),
        profiles: vec![],
        last_seen_ms: 0,
    };
    s.add_pc(rec).unwrap();
    s.set_pc_devices(PC, vec![device(A, PC), device(B, PC)], vec![]).unwrap();
    let g = Uuid::from_u128(99);
    s.put_full(PC, A, Cursor { generation: g, seq: 1 }, crate::store::fake_copy(b"copy of A")).unwrap();
    s.put_full(PC, B, Cursor { generation: g, seq: 1 }, crate::store::fake_copy(b"copy of B")).unwrap();
    s.hub_id()
}

/// Opens the store the way `serve` does and checks everything in it is there.
fn assert_intact(e: &Env, hub_id: Uuid) -> HubKey {
    let key = open_key(&e.cfg.keys_dir, &e.cfg.data_dir).unwrap();
    assert!(!e.cfg.keys_dir.join(NEW_KEY_FILE).exists(), "rotation not finished");
    assert_eq!(HubKey::load(&e.cfg.keys_dir).unwrap().id(), key.id());
    let (n, bad) = verify_dir(&e.cfg.data_dir, &[&key]).unwrap();
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!(n, 6, "hub.json, host.key, pcs.json, devices.json and two copies");
    let s = Store::open(&e.cfg, key.clone()).unwrap();
    assert_eq!(s.hub_id(), hub_id);
    assert_eq!(s.pcs().len(), 1);
    for (d, want) in [(A, &b"copy of A"[..]), (B, &b"copy of B"[..])] {
        match s.serve(d, None).first() {
            Some(Served::Full { bytes, .. }) => assert_eq!(bytes, &crate::store::fake_copy(want)),
            other => panic!("copy of {d} lost: {other:?}"),
        }
    }
    key
}

#[test]
fn offline_rotation_replaces_the_key() {
    let e = env();
    let id = fill(&e);
    let old = HubKey::load(&e.cfg.keys_dir).unwrap();
    assert_eq!(rotate_offline(&e.cfg.keys_dir, &e.cfg.data_dir).unwrap(), (6, false));
    let new = assert_intact(&e, id);
    assert_ne!(new.id(), old.id());
}

#[test]
fn crash_while_saving_the_new_key_keeps_the_old_one() {
    let e = env();
    let id = fill(&e);
    let old = HubKey::load(&e.cfg.keys_dir).unwrap();
    // step 1 died before its rename: only the temp file is there
    std::fs::write(e.cfg.keys_dir.join("hub.key.new.tmp"), b"half").unwrap();
    assert_eq!(assert_intact(&e, id).id(), old.id());
}

#[test]
fn crash_before_any_file_was_rewritten_finishes() {
    let e = env();
    let id = fill(&e);
    let (new, resumed) = begin(&e.cfg.keys_dir).unwrap();
    assert!(!resumed);
    assert_eq!(assert_intact(&e, id).id(), new.id());
}

#[test]
fn crash_halfway_finishes_at_next_start() {
    let e = env();
    let id = fill(&e);
    let old = HubKey::load(&e.cfg.keys_dir).unwrap();
    let (new, _) = begin(&e.cfg.keys_dir).unwrap();
    // three of the six files rewritten, then the crash
    for path in crate::store::files(&e.cfg.data_dir).unwrap().iter().take(3) {
        let plain = crypt::read(&old, path).unwrap();
        crypt::write(&new, path, &plain).unwrap();
    }
    // both keys are needed now, and enough
    assert!(verify_dir(&e.cfg.data_dir, &[&old]).unwrap().1.len() == 3);
    assert!(verify_dir(&e.cfg.data_dir, &[&old, &new]).unwrap().1.is_empty());
    // `rotate-key` run again (offline) resumes it instead of starting a third key
    assert!(begin(&e.cfg.keys_dir).unwrap().1);
    assert_eq!(assert_intact(&e, id).id(), new.id());
}

#[test]
fn crash_before_the_final_rename_finishes() {
    let e = env();
    let id = fill(&e);
    let old = HubKey::load(&e.cfg.keys_dir).unwrap();
    let (new, _) = begin(&e.cfg.keys_dir).unwrap();
    assert_eq!(rekey_dir(&e.cfg.data_dir, &old, &new).unwrap(), 6);
    assert_eq!(rekey_dir(&e.cfg.data_dir, &old, &new).unwrap(), 0, "a second pass rewrites nothing");
    assert_eq!(assert_intact(&e, id).id(), new.id());
}

#[test]
fn live_rotation_and_a_failed_one() {
    let e = env();
    let id = fill(&e);
    let s = Store::open(&e.cfg, HubKey::load(&e.cfg.keys_dir).unwrap()).unwrap();
    let (k1, _) = begin(&e.cfg.keys_dir).unwrap();
    assert_eq!(s.rekey(k1.clone()).unwrap(), 6);
    HubKey::commit_new(&e.cfg.keys_dir).unwrap();
    assert!(matches!(s.serve(A, None).first(), Some(Served::Full { .. })));

    // a damaged copy stops the next rotation halfway: the store keeps reading both keys
    let full_b = e.cfg.data_dir.join("copies").join(format!("{B}.full"));
    let saved_b = std::fs::read(&full_b).unwrap();
    std::fs::write(&full_b, b"not encrypted").unwrap();
    let (k2, _) = begin(&e.cfg.keys_dir).unwrap();
    assert!(s.rekey(k2.clone()).is_err());
    assert!(matches!(s.serve(A, None).first(), Some(Served::Full { .. })), "copy A lost after a failed rotation");
    // a different key is refused until this rotation is finished
    assert!(s.rekey(HubKey::generate()).is_err());
    // repaired: the same rotation resumes
    std::fs::write(&full_b, saved_b).unwrap();
    s.rekey(k2.clone()).unwrap();
    HubKey::commit_new(&e.cfg.keys_dir).unwrap();
    drop(s);
    assert_eq!(assert_intact(&e, id).id(), k2.id());
}
