use std::collections::HashMap;
use std::os::windows::ffi::OsStrExt;

use seek_core::{DeviceIndex, ATTR_DIRECTORY, ATTR_HIDDEN};
use seek_scan::scan_walk;
use windows_sys::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};

/// `a/b/c.txt` relative to the walk root, built from the index's parent links.
fn rel_path(ix: &DeviceIndex, row: u32) -> String {
    ix.full_path(row, "R:\\")
        .trim_start_matches("R:\\")
        .replace('\\', "/")
}

#[test]
fn walks_tree_with_parents_sizes_and_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    std::fs::write(root.join("a/b/c.txt"), b"hello").unwrap();
    std::fs::write(root.join("a/Ünï.md"), b"").unwrap();
    let secret = root.join(".secret");
    std::fs::write(&secret, b"x").unwrap();
    let wide: Vec<u16> = secret.as_os_str().encode_wide().chain(Some(0)).collect();
    assert_ne!(
        unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN) },
        0
    );

    let calls = std::sync::atomic::AtomicU64::new(0);
    let r = scan_walk(root, &|_| {
        calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    })
    .unwrap();
    let ix = &r.index;
    assert_eq!(ix.root_id(), 0);
    assert_eq!(r.skipped, 0);
    assert!(r.checkpoint.is_none());
    assert_eq!(ix.live_count(), 5, "ids unique, all rows live");
    assert!(calls.load(std::sync::atomic::Ordering::Relaxed) >= 1);
    assert!((0..5u32).all(|row| ix.id(row) != 0));

    let by_path: HashMap<String, u32> = (0..5u32).map(|row| (rel_path(ix, row), row)).collect();
    let mut paths: Vec<_> = by_path.keys().cloned().collect();
    paths.sort();
    assert_eq!(paths, [".secret", "a", "a/b", "a/b/c.txt", "a/Ünï.md"]);

    assert_eq!(ix.size(by_path["a/b/c.txt"]), 5);
    assert_eq!(ix.attrs(by_path["a"]) & ATTR_DIRECTORY, ATTR_DIRECTORY);
    assert_eq!(ix.size(by_path["a"]), 0);
    assert_eq!(ix.attrs(by_path[".secret"]) & ATTR_HIDDEN, ATTR_HIDDEN);
    assert!(ix.modified(by_path["a/b/c.txt"]) > 1_600_000_000_000);
}

#[test]
fn deep_and_wide_tree_keeps_every_parent_link() {
    // Siblings with subtrees force the walker to unwind its ancestor chain many times.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut expected = Vec::new();
    for a in 0..4 {
        for b in 0..3 {
            let p = format!("d{a}/e{b}/f/g");
            std::fs::create_dir_all(root.join(&p)).unwrap();
            for c in 0..3 {
                let f = format!("{p}/file{c}.txt");
                std::fs::write(root.join(&f), b"x").unwrap();
                expected.push(f);
            }
            expected.extend([format!("d{a}/e{b}"), format!("d{a}/e{b}/f"), p]);
        }
        expected.push(format!("d{a}"));
        std::fs::write(root.join(format!("d{a}/top.txt")), b"").unwrap();
        expected.push(format!("d{a}/top.txt"));
    }
    expected.sort();
    let r = scan_walk(root, &|_| {}).unwrap();
    let ix = &r.index;
    let mut got: Vec<String> = (0..ix.len() as u32).map(|row| rel_path(ix, row)).collect();
    got.sort();
    assert_eq!(got, expected);
    assert_eq!(r.skipped, 0);
}

#[test]
fn missing_root_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let r = scan_walk(&dir.path().join("nope"), &|_| {});
    assert!(matches!(r, Err(seek_scan::ScanError::NotFound)));
}
