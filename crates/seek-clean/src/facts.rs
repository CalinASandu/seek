//! What the engine asks of the outside world, as traits, plus the real `FileFacts`.
//! Tests swap in-memory fakes for `Recycler`, `Elevator` and `ClaimCheck`.

use std::fs::File;
use std::io::Read;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use windows::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};

use crate::model::{ItemOutcome, JobItem, Lock};
use crate::paths::open_query;

pub trait Recycler: Send + Sync {
    /// Per-item results in order. Stops at the first item it can't confirm in the bin.
    fn recycle(&self, paths: &[PathBuf]) -> Vec<Result<String, String>>;
}

pub trait Elevator: Send + Sync {
    /// Removes these items with admin rights, one prompt for the whole call. The real elevator
    /// adds the user's SID itself.
    fn run(&self, items: &[JobItem]) -> Result<Vec<ItemOutcome>, String>;
}

pub trait FileFacts: Send + Sync {
    /// Read from the disk, never the index. Never follows reparse points.
    fn stat(&self, path: &Path) -> Option<Stat>;
    /// Refuses (Err) to read a cloud placeholder, a reparse point or a folder.
    fn same_bytes(&self, a: &Path, b: &Path) -> Result<bool, String>;
    /// Volume serial and file ID.
    fn file_id(&self, path: &Path) -> Option<(u32, u64)>;
    /// The pre-checks of the spec (drive type, bin settings, capacity, path length).
    fn recycle_block(&self, path: &Path, batch_bytes: u64) -> Option<Lock>;
}

pub trait ClaimCheck: Send + Sync {
    /// Some(who) if an app owns this folder or a program is running inside it.
    fn is_claimed(&self, folder: &Path) -> Option<String>;
}

/// What the disk says about an item. For a folder, `size` and `files` cover everything under it
/// and `modified_ms` is the newest change anywhere under it. `cloud` is true if the item (or
/// anything under a folder) is an online-only placeholder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stat {
    pub size: u64,
    pub modified_ms: i64,
    pub is_dir: bool,
    pub files: u32,
    pub reparse: bool,
    pub cloud: bool,
}

const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_REPARSE: u32 = 0x400;
const ATTR_OFFLINE: u32 = 0x1000;
const ATTR_RECALL_ON_OPEN: u32 = 0x4_0000;
const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

fn is_cloud(attrs: u32) -> bool {
    attrs & (ATTR_OFFLINE | ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS) != 0
}

fn modified_ms(md: &std::fs::Metadata) -> i64 {
    // FILETIME counts 100 ns from 1601; the unix epoch is 11,644,473,600 s later.
    (md.last_write_time() / 10_000) as i64 - 11_644_473_600_000
}

/// The real thing, reading the disk.
pub struct RealFacts;

impl FileFacts for RealFacts {
    fn stat(&self, path: &Path) -> Option<Stat> {
        // symlink_metadata opens with FILE_FLAG_OPEN_REPARSE_POINT, so a link is described, not followed.
        let md = std::fs::symlink_metadata(path).ok()?;
        let attrs = md.file_attributes();
        let reparse = attrs & ATTR_REPARSE != 0;
        let is_dir = attrs & ATTR_DIRECTORY != 0;
        let mut s = Stat {
            size: md.len(),
            modified_ms: modified_ms(&md),
            is_dir,
            files: 1,
            reparse,
            cloud: is_cloud(attrs),
        };
        if is_dir && !reparse {
            walk(path, &mut s)?;
        }
        Some(s)
    }

    fn same_bytes(&self, a: &Path, b: &Path) -> Result<bool, String> {
        let mut sizes = [0u64; 2];
        for (i, p) in [a, b].into_iter().enumerate() {
            let s = self
                .stat(p)
                .ok_or_else(|| format!("Cannot read {}", p.display()))?;
            if s.is_dir || s.reparse || s.cloud {
                return Err(format!("{} is not an ordinary local file", p.display()));
            }
            sizes[i] = s.size;
        }
        if sizes[0] != sizes[1] {
            return Ok(false);
        }
        let (mut fa, mut fb) = (open(a)?, open(b)?);
        let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
        loop {
            let na = fill(&mut fa, &mut ba)?;
            let nb = fill(&mut fb, &mut bb)?;
            if na != nb || ba[..na] != bb[..nb] {
                return Ok(false);
            }
            if na == 0 {
                return Ok(true);
            }
        }
    }

    fn file_id(&self, path: &Path) -> Option<(u32, u64)> {
        let h = open_query(path)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `h` is a valid handle and `info` a valid out-structure.
        unsafe { GetFileInformationByHandle(h.0, &mut info) }.ok()?;
        Some((
            info.dwVolumeSerialNumber,
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ))
    }

    /// Filled in by the recycle pre-checks task.
    fn recycle_block(&self, _path: &Path, _batch_bytes: u64) -> Option<Lock> {
        None
    }
}

/// Walks a folder in this thread without following links. A link inside is neither counted nor
/// entered (but its own change time counts). Any unreadable entry fails the whole walk: an
/// unknown folder is not one to remove.
fn walk(root: &Path, s: &mut Stat) -> Option<()> {
    s.files = 0;
    s.size = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).ok()? {
            let entry = entry.ok()?;
            let md = entry.metadata().ok()?;
            let attrs = md.file_attributes();
            s.modified_ms = s.modified_ms.max(modified_ms(&md));
            s.cloud |= is_cloud(attrs);
            if attrs & ATTR_REPARSE != 0 {
                continue;
            }
            if attrs & ATTR_DIRECTORY != 0 {
                stack.push(entry.path());
            } else {
                s.files = s.files.saturating_add(1);
                s.size = s.size.saturating_add(md.len());
            }
        }
    }
    Some(())
}

fn open(p: &Path) -> Result<File, String> {
    File::open(p).map_err(|e| format!("Cannot read {}: {e}", p.display()))
}

fn fill(f: &mut File, buf: &mut [u8]) -> Result<usize, String> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("Read failed: {e}")),
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_of_a_folder_sums_files_and_skips_a_junction() {
        let d = tempfile::tempdir().unwrap();
        let outside = d.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("big.bin"), vec![0u8; 5000]).unwrap();
        let f = d.path().join("f");
        std::fs::create_dir_all(f.join("sub")).unwrap();
        std::fs::write(f.join("a.txt"), b"12345").unwrap();
        std::fs::write(f.join("sub").join("b.txt"), b"123").unwrap();
        let s = RealFacts.stat(&f).unwrap();
        assert_eq!((s.size, s.files, s.is_dir, s.reparse), (8, 2, true, false));
        assert!(crate::paths::make_junction(&f.join("link"), &outside));
        let s2 = RealFacts.stat(&f).unwrap();
        assert_eq!(
            (s2.size, s2.files),
            (8, 2),
            "junction neither counted nor followed"
        );
        let l = RealFacts.stat(&f.join("link")).unwrap();
        assert!(l.reparse);
        assert!(RealFacts
            .same_bytes(&f.join("link"), &f.join("a.txt"))
            .is_err());
    }

    #[test]
    fn same_bytes_compares_contents_and_refuses_folders() {
        let d = tempfile::tempdir().unwrap();
        let (a, b, c) = (d.path().join("a"), d.path().join("b"), d.path().join("c"));
        let mut big = vec![7u8; 200_000];
        std::fs::write(&a, &big).unwrap();
        std::fs::write(&b, &big).unwrap();
        *big.last_mut().unwrap() = 8;
        std::fs::write(&c, &big).unwrap();
        assert_eq!(RealFacts.same_bytes(&a, &b), Ok(true));
        assert_eq!(RealFacts.same_bytes(&a, &c), Ok(false));
        assert!(RealFacts.same_bytes(&a, d.path()).is_err());
        assert!(RealFacts.same_bytes(&a, &d.path().join("none")).is_err());
    }

    #[test]
    fn file_id_differs_for_copies_and_matches_for_hard_links() {
        let d = tempfile::tempdir().unwrap();
        let (a, b, c) = (d.path().join("a"), d.path().join("b"), d.path().join("c"));
        std::fs::write(&a, b"x").unwrap();
        std::fs::copy(&a, &b).unwrap();
        std::fs::hard_link(&a, &c).unwrap();
        let id = |p: &Path| RealFacts.file_id(p).unwrap();
        assert_ne!(id(&a), id(&b));
        assert_eq!(id(&a), id(&c));
        assert!(RealFacts.file_id(&d.path().join("none")).is_none());
    }
}
