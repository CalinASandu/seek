//! What the engine asks of the outside world, as traits, plus the real `FileFacts`.
//! Tests swap in-memory fakes for `Recycler`, `Elevator` and `ClaimCheck`.

use std::fs::File;
use std::io::Read;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::Storage::FileSystem::{
    GetDriveTypeW, GetFileInformationByHandle, GetVolumeNameForVolumeMountPointW, BY_HANDLE_FILE_INFORMATION,
};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::Shell::{SHQueryRecycleBinW, SHQUERYRBINFO};

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

    /// Reads the facts from this PC, then decides with `decide`. Anything it cannot read blocks.
    fn recycle_block(&self, path: &Path, batch_bytes: u64) -> Option<Lock> {
        use std::os::windows::ffi::OsStrExt;
        let path_units = path.as_os_str().encode_wide().count();
        let Ok(md) = std::fs::symlink_metadata(path) else {
            return Some(Lock::UnsupportedPath);
        };
        // The drive of the real location: a link into another volume is judged by that volume.
        let drive = crate::paths::real_location(path).and_then(|r| drive_root(&r));
        let kind = match &drive {
            Some(root) => DriveType::from_raw(drive_type(root)),
            None => DriveType::Unknown,
        };
        let drive_ok = drive_allows(kind);
        let (bin_query_ok, settings) = match (&drive, drive_ok) {
            (Some(root), true) => (bin_query_succeeds(root), read_bin_settings(root)),
            _ => (false, BinSettings::default()),
        };
        decide(&Inputs {
            path_units,
            attrs: md.file_attributes(),
            drive: kind,
            bin_query_ok,
            settings,
            batch_bytes,
        })
    }
}

/// The longest original path, in UTF-16 units, that Seek will send to the bin. The bin stores the
/// original path in its `$I` record, whose limit is not documented precisely. 259 is MAX_PATH
/// minus the terminator, so this is conservative: some longer paths may recycle fine and are
/// refused anyway, which only costs the user a manual removal.
pub(crate) const MAX_ORIGINAL_PATH_UNITS: usize = 259;

/// A batch above this share of the bin's `MaxCapacity` is refused. Windows does not document the
/// exact point at which it deletes permanently instead, so the margin is deliberate.
pub(crate) const CAPACITY_MARGIN_PERCENT: u128 = 90;

/// `MaxCapacity` in the registry is in megabytes.
const BYTES_PER_CAPACITY_UNIT: u128 = 1024 * 1024;

/// `GetDriveTypeW`, as a type that cannot be mistaken for a number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DriveType {
    Fixed,
    Removable,
    Remote,
    CdRom,
    RamDisk,
    /// DRIVE_NO_ROOT_DIR, DRIVE_UNKNOWN, or a root that could not be worked out.
    Unknown,
}

impl DriveType {
    pub(crate) fn from_raw(raw: u32) -> DriveType {
        match raw {
            2 => DriveType::Removable,
            3 => DriveType::Fixed,
            4 => DriveType::Remote,
            5 => DriveType::CdRom,
            6 => DriveType::RamDisk,
            _ => DriveType::Unknown,
        }
    }
}

/// Only a fixed local drive has a bin Seek can rely on.
pub(crate) fn drive_allows(kind: DriveType) -> bool {
    kind == DriveType::Fixed
}

/// The values of `BitBucket\Volume\{guid}` that matter. `None` = the value is absent (or, for
/// `nuke_on_delete`, unreadable, which is stored as on).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BinSettings {
    pub(crate) nuke_on_delete: Option<u32>,
    /// Megabytes.
    pub(crate) max_capacity_mb: Option<u32>,
}

/// True when the bin settings alone forbid recycling this batch. An absent `NukeOnDelete` is
/// Windows' default (off). An absent or unreadable `MaxCapacity` blocks.
pub(crate) fn bin_settings_block(s: &BinSettings, batch_bytes: u64) -> bool {
    if s.nuke_on_delete.unwrap_or(0) != 0 {
        return true;
    }
    let Some(mb) = s.max_capacity_mb else {
        return true;
    };
    let capacity = u128::from(mb) * BYTES_PER_CAPACITY_UNIT;
    u128::from(batch_bytes) * 100 > capacity * CAPACITY_MARGIN_PERCENT
}

/// Everything `decide` needs, already read.
pub(crate) struct Inputs {
    pub(crate) path_units: usize,
    pub(crate) attrs: u32,
    pub(crate) drive: DriveType,
    pub(crate) bin_query_ok: bool,
    pub(crate) settings: BinSettings,
    pub(crate) batch_bytes: u64,
}

/// The pre-checks of the spec, as a pure function. The first failing one names the lock.
pub(crate) fn decide(i: &Inputs) -> Option<Lock> {
    if i.path_units > MAX_ORIGINAL_PATH_UNITS {
        return Some(Lock::UnsupportedPath);
    }
    if is_cloud(i.attrs) {
        return Some(Lock::CloudPlaceholder);
    }
    if !drive_allows(i.drive) || !i.bin_query_ok || bin_settings_block(&i.settings, i.batch_bytes) {
        return Some(Lock::NoRecycleBin);
    }
    None
}

/// The volume GUID inside a `\\?\Volume{guid}\` name, without braces.
pub(crate) fn volume_guid(volume_name: &str) -> Option<String> {
    let g = volume_name.split('{').nth(1)?.split('}').next()?;
    (!g.is_empty()).then(|| g.to_string())
}

/// `c:` from a `canon` path becomes `c:\`. Anything not shaped like a drive letter is None.
fn drive_root(canon_path: &str) -> Option<String> {
    let b = canon_path.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':').then(|| format!("{}\\", &canon_path[..2]))
}

fn wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn drive_type(root: &str) -> u32 {
    // SAFETY: `wide_z` is NUL-terminated and outlives the call.
    unsafe { GetDriveTypeW(PCWSTR(wide_z(root).as_ptr())) }
}

/// `SHQueryRecycleBinW` must succeed for the drive; a failure means Seek cannot vouch for its bin.
fn bin_query_succeeds(root: &str) -> bool {
    let mut info = SHQUERYRBINFO {
        cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a valid out-structure with its size set; the string is NUL-terminated.
    unsafe { SHQueryRecycleBinW(PCWSTR(wide_z(root).as_ptr()), &mut info) }.is_ok()
}

const BIT_BUCKET_VOLUMES: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume";

/// Reads the user's bin settings for the volume holding `root`. Read only. If the volume's GUID
/// or the key can't be found the result is all `None`, which blocks (no `MaxCapacity`).
fn read_bin_settings(root: &str) -> BinSettings {
    let mut name = [0u16; 64];
    // SAFETY: `name` is a valid buffer, the root NUL-terminated.
    if unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(wide_z(root).as_ptr()), &mut name) }.is_err() {
        return BinSettings::default();
    }
    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    let Some(guid) = volume_guid(&String::from_utf16_lossy(&name[..end])) else {
        return BinSettings::default();
    };
    let key = wide_z(&format!(r"{BIT_BUCKET_VOLUMES}\{{{guid}}}"));
    BinSettings {
        // Unreadable (not merely absent) counts as on.
        nuke_on_delete: read_dword(&key, "NukeOnDelete").unwrap_or(Some(1)),
        max_capacity_mb: read_dword(&key, "MaxCapacity").unwrap_or(None),
    }
}

/// Ok(None) = value absent; Err = present or key present but unreadable.
fn read_dword(key: &[u16], value: &str) -> Result<Option<u32>, ()> {
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: `key` and the value name are NUL-terminated; `data`/`size` describe a u32.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(wide_z(value).as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut data as *mut u32).cast()),
            Some(&mut size),
        )
    };
    if rc == ERROR_SUCCESS {
        Ok(Some(data))
    } else if rc == ERROR_FILE_NOT_FOUND || rc == ERROR_PATH_NOT_FOUND {
        Ok(None)
    } else {
        Err(())
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

    const MB: u64 = 1024 * 1024;

    fn ok_inputs() -> Inputs {
        Inputs {
            path_units: 40,
            attrs: 0x20,
            drive: DriveType::Fixed,
            bin_query_ok: true,
            settings: BinSettings {
                nuke_on_delete: Some(0),
                max_capacity_mb: Some(1000),
            },
            batch_bytes: 10 * MB,
        }
    }

    #[test]
    fn an_ordinary_item_is_allowed() {
        assert_eq!(decide(&ok_inputs()), None);
    }

    #[test]
    fn removable_network_and_unknown_drives_block() {
        assert!(drive_allows(DriveType::from_raw(3)));
        for raw in [0, 1, 2, 4, 5, 6, 99] {
            assert!(!drive_allows(DriveType::from_raw(raw)), "drive type {raw}");
            let i = Inputs {
                drive: DriveType::from_raw(raw),
                ..ok_inputs()
            };
            assert_eq!(decide(&i), Some(Lock::NoRecycleBin), "drive type {raw}");
        }
    }

    #[test]
    fn nuke_on_delete_blocks() {
        for (nuke, blocks) in [(Some(0), false), (None, false), (Some(1), true), (Some(7), true)] {
            let i = Inputs {
                settings: BinSettings {
                    nuke_on_delete: nuke,
                    max_capacity_mb: Some(1000),
                },
                ..ok_inputs()
            };
            assert_eq!(decide(&i) == Some(Lock::NoRecycleBin), blocks, "NukeOnDelete {nuke:?}");
        }
    }

    #[test]
    fn a_batch_over_max_capacity_blocks() {
        let at = |mb: Option<u32>, bytes: u64| Inputs {
            settings: BinSettings {
                nuke_on_delete: Some(0),
                max_capacity_mb: mb,
            },
            batch_bytes: bytes,
            ..ok_inputs()
        };
        // 90% of 1000 MB is 900 MB.
        assert_eq!(decide(&at(Some(1000), 900 * MB)), None);
        assert_eq!(decide(&at(Some(1000), 900 * MB + 1)), Some(Lock::NoRecycleBin));
        assert_eq!(decide(&at(Some(1000), 2000 * MB)), Some(Lock::NoRecycleBin));
        // The capacity is in MB: a 1000 MB batch is not "tiny" next to a 1000 value.
        assert_eq!(decide(&at(Some(1000), 1000 * MB)), Some(Lock::NoRecycleBin));
        // Unreadable capacity refuses, even for an empty batch.
        assert_eq!(decide(&at(None, 0)), Some(Lock::NoRecycleBin));
        // Huge values do not overflow.
        assert_eq!(decide(&at(Some(u32::MAX), u64::MAX)), Some(Lock::NoRecycleBin));
    }

    #[test]
    fn a_failed_bin_query_blocks() {
        let i = Inputs {
            bin_query_ok: false,
            ..ok_inputs()
        };
        assert_eq!(decide(&i), Some(Lock::NoRecycleBin));
    }

    #[test]
    fn a_too_long_path_blocks_with_its_own_reason() {
        let at = |n| Inputs {
            path_units: n,
            ..ok_inputs()
        };
        assert_eq!(decide(&at(MAX_ORIGINAL_PATH_UNITS)), None);
        assert_eq!(decide(&at(MAX_ORIGINAL_PATH_UNITS + 1)), Some(Lock::UnsupportedPath));
    }

    #[test]
    fn cloud_placeholder_attributes_block() {
        for attr in [0x40_0000, 0x4_0000, 0x1000] {
            let i = Inputs {
                attrs: 0x20 | attr,
                ..ok_inputs()
            };
            assert_eq!(decide(&i), Some(Lock::CloudPlaceholder), "attribute {attr:#x}");
        }
    }

    #[test]
    fn the_volume_guid_comes_out_of_the_volume_name() {
        assert_eq!(
            volume_guid(r"\\?\Volume{dffe249c-27c3-48f1-825e-f3845200c32e}\").as_deref(),
            Some("dffe249c-27c3-48f1-825e-f3845200c32e")
        );
        assert_eq!(volume_guid(r"\\?\Volume{}\"), None);
        assert_eq!(volume_guid(r"C:\"), None);
    }

    #[test]
    fn only_a_drive_letter_path_has_a_root() {
        assert_eq!(drive_root(r"c:\users\a").as_deref(), Some(r"c:\"));
        assert_eq!(drive_root(r"\\server\share\f"), None);
        assert_eq!(drive_root(""), None);
    }

    /// This PC has the bin settings for its fixed drives; a normal file there must be allowed.
    #[test]
    fn a_normal_temp_file_on_this_pc_is_allowed() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.txt");
        std::fs::write(&f, b"x").unwrap();
        assert_eq!(RealFacts.recycle_block(&f, 1), None);
        assert_eq!(
            RealFacts.recycle_block(&d.path().join("missing"), 1),
            Some(Lock::UnsupportedPath)
        );
    }

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
