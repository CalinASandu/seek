use seek_core::*;

fn e(id: u64, parent: u64, name: &str, dir: bool) -> RawEntry {
    RawEntry {
        id,
        parent_id: parent,
        name: name.into(),
        size: if dir { 0 } else { id * 10 },
        modified: 1_700_000_000_000,
        created: 1_600_000_000_000,
        attrs: if dir { ATTR_DIRECTORY } else { 0 },
    }
}

fn sample() -> DeviceIndex {
    DeviceIndex::from_raw(
        5,
        vec![
            e(5, 5, ".", true),
            e(10, 5, "Users", true),
            e(11, 10, "Me", true),
            e(12, 11, "Report.PDF", false),
            e(13, 11, "Ünïcödé İ.txt", false),
            e(14, 999, "orphan.txt", false),
        ],
    )
}

fn hdr() -> IndexHeader {
    IndexHeader {
        format_version: INDEX_FORMAT_VERSION,
        device_id: uuid::Uuid::from_u128(1),
        host_id: uuid::Uuid::from_u128(2),
        checkpoint: Some(Checkpoint {
            journal_id: 7,
            next_usn: 4242,
        }),
        saved_at: 1_700_000_000_123,
        rows: 4,
        sync_cursor: None,
    }
}

fn assert_same(a: &DeviceIndex, b: &DeviceIndex) {
    assert_eq!(a.root_id(), b.root_id());
    assert_eq!(a.live_count(), b.live_count());
    assert_eq!(a.checkpoint, b.checkpoint);
    for row in 0..a.len() as u32 {
        if !a.is_alive(row) {
            continue;
        }
        let r = b.row_of(a.id(row)).expect("row survives roundtrip");
        assert_eq!(a.name(row), b.name(r));
        assert_eq!(a.name_lower(row), b.name_lower(r));
        assert_eq!(a.full_path(row, "C:\\"), b.full_path(r, "C:\\"));
        assert_eq!(a.size(row), b.size(r));
        assert_eq!(a.attrs(row), b.attrs(r));
        assert_eq!(a.modified(row), b.modified(r));
        assert_eq!(a.created(row), b.created(r));
    }
}

#[test]
fn roundtrip() {
    let mut ix = sample();
    ix.checkpoint = hdr().checkpoint;
    let bytes = encode_index(&hdr(), &mut ix).unwrap();
    let (h, back) = decode_index(&bytes).unwrap();
    assert_eq!(h, hdr());
    assert_same(&ix, &back);
}

#[test]
fn roundtrip_after_changes_compacts() {
    let mut ix = sample();
    ix.apply(&[
        Change::Upsert(e(12, 10, "renamed.pdf", false)),
        Change::Delete { id: 13 },
    ]);
    ix.checkpoint = hdr().checkpoint;
    let mut h = hdr();
    h.rows = 0;
    let bytes = encode_index(&h, &mut ix).unwrap();
    let (h2, back) = decode_index(&bytes).unwrap();
    assert_eq!(h2.rows, 3);
    assert_eq!(back.len(), 3);
    assert_same(&ix, &back);
}

#[test]
fn rejects_garbage() {
    assert!(decode_index(b"").is_err());
    assert!(decode_index(b"SEEKIDX\0").is_err());
    let mut bytes = encode_index(&hdr(), &mut sample()).unwrap();
    bytes[8] = 99;
    match decode_index(&bytes) {
        Err(Error::Format(m)) => assert!(m.contains("unsupported version 99"), "{m}"),
        other => panic!("unexpected: {other:?}"),
    }
    let good = encode_index(&hdr(), &mut sample()).unwrap();
    for cut in [9, 20, good.len() / 2, good.len() - 1] {
        assert!(decode_index(&good[..cut]).is_err(), "cut {cut}");
    }
    // bit flips must never panic
    for i in 0..good.len() {
        let mut b = good.clone();
        b[i] ^= 0xFF;
        let _ = decode_index(&b);
    }
}

#[test]
fn save_load_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dev.seekidx");
    let mut ix = sample();
    ix.checkpoint = hdr().checkpoint;
    save_index(&path, &hdr(), &mut ix).unwrap();
    save_index(&path, &hdr(), &mut ix).unwrap(); // overwrite
    let (h, back) = load_index(&path).unwrap();
    assert_eq!(h, hdr());
    assert_same(&ix, &back);
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 1, "no .tmp left: {names:?}");
}

#[test]
fn registry_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let mut reg = DeviceRegistry::load_or_create(dir.path()).unwrap();
    assert!(reg.devices.is_empty());
    let host = reg.host_id;
    let ident = DeviceIdentity::Volume {
        serial: 0xABCD,
        filesystem: "NTFS".into(),
    };
    let id = uuid::Uuid::new_v4();
    reg.add(Device {
        id,
        identity: ident.clone(),
        display_name: "System".into(),
        label: "".into(),
        filesystem: "NTFS".into(),
        kind: DeviceKind::Fixed,
        total_bytes: 10,
        free_bytes: 5,
        last_mount_point: "C:\\".into(),
        indexed_by_host: host,
        first_seen: 1,
        last_seen: 2,
        index_updated_at: 3,
        state: DeviceState::Offline,
        entry_count: 4,
        error: None,
    });
    reg.save(dir.path()).unwrap();
    let again = DeviceRegistry::load_or_create(dir.path()).unwrap();
    assert_eq!(again.host_id, host);
    assert_eq!(again.find_by_identity(&ident).unwrap().id, id);
    assert!(again
        .find_by_identity(&DeviceIdentity::Share {
            unc: "\\\\x\\y".into()
        })
        .is_none());
    let mut again = again;
    again.get_mut(id).unwrap().display_name = "Renamed".into();
    assert_eq!(again.get(id).unwrap().display_name, "Renamed");
    assert!(again.remove(id).is_some());
    assert!(again.get(id).is_none());
}

#[test]
fn level_one_encoding_roundtrips_and_changes_serialize() {
    let mut ix = synth_index(50_000, 1);
    let copy = ix.clone();
    let hdr = IndexHeader {
        format_version: 0,
        device_id: uuid::Uuid::nil(),
        host_id: uuid::Uuid::nil(),
        checkpoint: None,
        saved_at: 0,
        rows: 0,
        sync_cursor: None,
    };
    let bytes = encode_index_level(&hdr, &mut ix, 1).unwrap();
    let (back_hdr, back) = decode_index(&bytes).unwrap();
    assert_eq!(back.live_count(), copy.live_count());
    assert_eq!(back_hdr.sync_cursor, None);
    let change = Change::Upsert(e(3, 1, "a.txt", false));
    let json = serde_json::to_string(&change).unwrap();
    assert_eq!(serde_json::from_str::<Change>(&json).unwrap(), change);
}
