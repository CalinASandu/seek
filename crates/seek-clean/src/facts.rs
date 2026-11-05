//! What the engine asks of the outside world, as traits, plus the real `FileFacts`.
//! Tests swap in-memory fakes for `Recycler`, `Elevator` and `ClaimCheck`.

use std::fs::File;
use std::io::Read;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetFileInformationByHandle, GetVolumeInformationW,
    GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, BY_HANDLE_FILE_INFORMATION,
};
use windows::Win32::System::Registry::{RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};
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
        let written_units = path.as_os_str().encode_wide().count();
        let name_units = path.file_name().map_or(0, |n| n.encode_wide().count());
        let Ok(md) = std::fs::symlink_metadata(path) else {
            return Some(Lock::UnsupportedPath);
        };
        let attrs = md.file_attributes();
        let is_dir = attrs & ATTR_DIRECTORY != 0 && attrs & ATTR_REPARSE == 0;
        let Ok(inner_suffix) = longest_inner_suffix(path, is_dir) else {
            return Some(Lock::UnsupportedPath); // an unreadable entry: an unknown folder is not one to remove
        };
        // The drive of the real location: a link into another volume is judged by that volume.
        let real = crate::paths::real_location(path);
        let real_units = real.as_deref().map_or(0, |r| r.encode_utf16().count());
        let drive = real.as_deref().and_then(drive_root);
        let mut i = Inputs {
            top_units: written_units.max(real_units),
            name_units,
            inner_suffix,
            attrs,
            drive: DriveType::Unknown,
            same_root: real
                .as_deref()
                .is_some_and(|r| same_drive(&path.to_string_lossy(), r)),
            volume_root_ok: false,
            fs_ok: false,
            bin_used: None,
            settings: BinSettings::default(),
            policies: Policies::default(),
            volume_total: None,
            batch_bytes,
        };
        let Some(root) = drive else {
            return decide(&i);
        };
        i.drive = DriveType::from_raw(drive_type(&root));
        if !drive_allows(i.drive) {
            return decide(&i);
        }
        i.volume_root_ok = real.as_deref().is_some_and(|r| volume_path_name(r).is_some_and(|v| v.eq_ignore_ascii_case(&root)));
        i.fs_ok = file_system(&root).is_some_and(|f| fs_allows(&f));
        i.bin_used = bin_used_bytes(&root);
        i.settings = read_bin_settings(&root);
        i.policies = read_policies();
        if i.policies.recycle_bin_size != PolicyValue::Absent {
            i.volume_total = volume_total_bytes(&root);
        }
        decide(&i)
    }
}

/// The longest original path, in UTF-16 units, that Seek will send to the bin. The bin stores the
/// original path in its `$I` record, whose limit is not documented precisely. 259 is MAX_PATH
/// minus the terminator, so this is conservative: some longer paths may recycle fine and are
/// refused anyway, which only costs the user a manual removal.
pub(crate) const MAX_ORIGINAL_PATH_UNITS: usize = 259;

/// What the bin adds in front of a stored path (its `$Recycle.Bin\<SID>\$R...` location), in
/// UTF-16 units. A generous estimate: the SID is about 46 units and the rest about 25. The path
/// inside the bin is that prefix plus the item's path relative to the selected folder, and it too
/// must stay under `MAX_ORIGINAL_PATH_UNITS`.
pub(crate) const BIN_PREFIX_UNITS: usize = 80;

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

/// A policy value under `Policies\Explorer`, merged over HKCU and HKLM.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PolicyValue {
    #[default]
    Absent,
    Value(u32),
    /// Present but not readable as a DWORD.
    Unreadable,
}

impl PolicyValue {
    /// Combines two hives: unreadable wins, then `pick` chooses between two values.
    pub(crate) fn merge(self, other: PolicyValue, pick: fn(u32, u32) -> u32) -> PolicyValue {
        match (self, other) {
            (PolicyValue::Unreadable, _) | (_, PolicyValue::Unreadable) => PolicyValue::Unreadable,
            (PolicyValue::Absent, o) | (o, PolicyValue::Absent) => o,
            (PolicyValue::Value(a), PolicyValue::Value(b)) => PolicyValue::Value(pick(a, b)),
        }
    }
}

/// `NoRecycleFiles` (non-zero: files are deleted for good) and `RecycleBinSize` (percent of the
/// volume the bin may use).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Policies {
    pub(crate) no_recycle_files: PolicyValue,
    pub(crate) recycle_bin_size: PolicyValue,
}

/// Why settings, policies and the bin's size forbid recycling this batch. `NoRecycleBin` for a
/// setting, a policy, an unreadable or zero capacity, or a batch that alone exceeds the capacity.
/// `BinFull` when the batch fits but what is already in the bin leaves no room, or the bin's
/// size is unknown. An absent `NukeOnDelete` is Windows' default (off).
pub(crate) fn capacity_block(
    s: &BinSettings,
    p: &Policies,
    volume_total: Option<u64>,
    bin_used: Option<u64>,
    batch_bytes: u64,
) -> Option<Lock> {
    if s.nuke_on_delete.unwrap_or(0) != 0 {
        return Some(Lock::NoRecycleBin);
    }
    match p.no_recycle_files {
        PolicyValue::Absent | PolicyValue::Value(0) => {}
        PolicyValue::Value(_) | PolicyValue::Unreadable => return Some(Lock::NoRecycleBin),
    }
    let Some(mb) = s.max_capacity_mb else {
        return Some(Lock::NoRecycleBin);
    };
    let mut capacity = u128::from(mb) * BYTES_PER_CAPACITY_UNIT;
    match p.recycle_bin_size {
        PolicyValue::Absent => {}
        PolicyValue::Unreadable => return Some(Lock::NoRecycleBin),
        PolicyValue::Value(percent) => {
            let Some(total) = volume_total else {
                return Some(Lock::NoRecycleBin);
            };
            capacity = capacity.min(u128::from(total) * u128::from(percent) / 100);
        }
    }
    if capacity == 0 {
        return Some(Lock::NoRecycleBin);
    }
    // The batch alone must fit; if it does, what is already in the bin must leave room for it.
    let limit = capacity * CAPACITY_MARGIN_PERCENT;
    if u128::from(batch_bytes) * 100 > limit {
        return Some(Lock::NoRecycleBin);
    }
    let Some(used) = bin_used else {
        return Some(Lock::BinFull);
    };
    ((u128::from(used) + u128::from(batch_bytes)) * 100 > limit).then_some(Lock::BinFull)
}

/// True when the item (or anything inside it) has a path the bin cannot store. `top_units` is the
/// selected path (the longer of its written and real spellings), `name_units` its last segment,
/// and `inner_suffix` the longest path inside it, relative to it, if it is a folder with contents.
pub(crate) fn path_too_long(top_units: usize, name_units: usize, inner_suffix: Option<usize>) -> bool {
    let inner = inner_suffix.map_or(0, |s| s + 1); // the separator plus the relative path
    top_units + inner > MAX_ORIGINAL_PATH_UNITS
        || BIN_PREFIX_UNITS + name_units + inner > MAX_ORIGINAL_PATH_UNITS
}

/// Only NTFS and ReFS keep a bin Seek can rely on; Windows may delete from other formats for good.
pub(crate) fn fs_allows(name: &str) -> bool {
    name.eq_ignore_ascii_case("NTFS") || name.eq_ignore_ascii_case("ReFS")
}

/// True when both paths start with the same drive letter (case-insensitive). A subst drive or a
/// junction to another volume makes the written and real roots differ.
pub(crate) fn same_drive(written: &str, real: &str) -> bool {
    match (drive_root(written), drive_root(real)) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(&b),
        _ => false,
    }
}

/// Everything `decide` needs, already read.
pub(crate) struct Inputs {
    /// The longer of the written and real spellings of the path.
    pub(crate) top_units: usize,
    pub(crate) name_units: usize,
    /// Longest path inside a folder, relative to it.
    pub(crate) inner_suffix: Option<usize>,
    pub(crate) attrs: u32,
    pub(crate) drive: DriveType,
    /// The written path and the real path start on the same drive letter.
    pub(crate) same_root: bool,
    /// The real location's volume mount point is exactly the drive's `x:\`.
    pub(crate) volume_root_ok: bool,
    pub(crate) fs_ok: bool,
    /// Bytes in the drive's bin, `None` when `SHQueryRecycleBinW` failed.
    pub(crate) bin_used: Option<u64>,
    pub(crate) settings: BinSettings,
    pub(crate) policies: Policies,
    pub(crate) volume_total: Option<u64>,
    pub(crate) batch_bytes: u64,
}

/// The pre-checks of the spec, as a pure function. The first failing one names the lock.
pub(crate) fn decide(i: &Inputs) -> Option<Lock> {
    if path_too_long(i.top_units, i.name_units, i.inner_suffix) {
        return Some(Lock::PathTooLong);
    }
    if is_cloud(i.attrs) {
        return Some(Lock::CloudPlaceholder);
    }
    // A subst drive, a junction to another volume or a folder-mounted volume: the written path is
    // not a place Seek can reason about.
    if !i.same_root || !i.volume_root_ok {
        return Some(Lock::UnsupportedPath);
    }
    if !drive_allows(i.drive) || !i.fs_ok {
        return Some(Lock::NoRecycleBin);
    }
    capacity_block(&i.settings, &i.policies, i.volume_total, i.bin_used, i.batch_bytes)
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

/// The bin's current size in bytes from `SHQueryRecycleBinW`. A failed call (or a nonsense
/// negative size) is `None`, which blocks: Seek cannot vouch for a bin it cannot query.
fn bin_used_bytes(root: &str) -> Option<u64> {
    let mut info = SHQUERYRBINFO {
        cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a valid out-structure with its size set; the string is NUL-terminated.
    unsafe { SHQueryRecycleBinW(PCWSTR(wide_z(root).as_ptr()), &mut info) }.ok()?;
    u64::try_from(info.i64Size).ok()
}

/// The mount point of the volume holding `path`; `x:\` for a volume mounted as a drive letter.
fn volume_path_name(path: &str) -> Option<String> {
    let mut buf = [0u16; 1024];
    // SAFETY: NUL-terminated input, valid output buffer.
    unsafe { GetVolumePathNameW(PCWSTR(wide_z(path).as_ptr()), &mut buf) }.ok()?;
    let end = buf.iter().position(|&c| c == 0)?;
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// The file system name (`NTFS`) of the volume at `root`.
fn file_system(root: &str) -> Option<String> {
    let mut buf = [0u16; 64];
    // SAFETY: NUL-terminated input, valid output buffer.
    unsafe { GetVolumeInformationW(PCWSTR(wide_z(root).as_ptr()), None, None, None, None, Some(&mut buf)) }.ok()?;
    let end = buf.iter().position(|&c| c == 0)?;
    Some(String::from_utf16_lossy(&buf[..end]))
}

fn volume_total_bytes(root: &str) -> Option<u64> {
    let mut total = 0u64;
    // SAFETY: NUL-terminated input, valid out-pointer.
    unsafe { GetDiskFreeSpaceExW(PCWSTR(wide_z(root).as_ptr()), None, Some(&mut total), None) }.ok()?;
    Some(total)
}

const POLICIES_EXPLORER: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";

/// `NoRecycleFiles` and `RecycleBinSize` from both HKCU and HKLM, read only.
fn read_policies() -> Policies {
    let key = wide_z(POLICIES_EXPLORER);
    let one = |name: &str, pick: fn(u32, u32) -> u32| {
        let of = |hive| match read_dword(hive, &key, name) {
            Ok(None) => PolicyValue::Absent,
            Ok(Some(v)) => PolicyValue::Value(v),
            Err(()) => PolicyValue::Unreadable,
        };
        of(HKEY_CURRENT_USER).merge(of(HKEY_LOCAL_MACHINE), pick)
    };
    Policies {
        no_recycle_files: one("NoRecycleFiles", u32::max),
        recycle_bin_size: one("RecycleBinSize", u32::min),
    }
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
        nuke_on_delete: read_dword(HKEY_CURRENT_USER, &key, "NukeOnDelete").unwrap_or(Some(1)),
        max_capacity_mb: read_dword(HKEY_CURRENT_USER, &key, "MaxCapacity").unwrap_or(None),
    }
}

/// Ok(None) = value (or key) absent; Err = present but not readable as a DWORD.
fn read_dword(hive: HKEY, key: &[u16], value: &str) -> Result<Option<u32>, ()> {
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: `key` and the value name are NUL-terminated; `data`/`size` describe a u32.
    let rc = unsafe {
        RegGetValueW(
            hive,
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

/// The longest path inside a folder, in UTF-16 units relative to it (`sub\file.txt` is 12), or
/// `None` for a file or an empty folder. Links inside are counted but not entered. Any unreadable
/// entry is an Err: an unknown folder is not one to remove.
fn longest_inner_suffix(root: &Path, is_dir: bool) -> Result<Option<usize>, ()> {
    use std::os::windows::ffi::OsStrExt;
    if !is_dir {
        return Ok(None);
    }
    let mut longest: Option<usize> = None;
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, dir_suffix)) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|_| ())? {
            let entry = entry.map_err(|_| ())?;
            let md = entry.metadata().map_err(|_| ())?;
            let name = entry.file_name().encode_wide().count();
            let suffix = if dir_suffix == 0 { name } else { dir_suffix + 1 + name };
            longest = Some(longest.map_or(suffix, |l| l.max(suffix)));
            let attrs = md.file_attributes();
            if attrs & ATTR_DIRECTORY != 0 && attrs & ATTR_REPARSE == 0 {
                stack.push((entry.path(), suffix));
            }
        }
    }
    Ok(longest)
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
            top_units: 40,
            name_units: 10,
            inner_suffix: None,
            attrs: 0x20,
            drive: DriveType::Fixed,
            same_root: true,
            volume_root_ok: true,
            fs_ok: true,
            bin_used: Some(0),
            settings: BinSettings {
                nuke_on_delete: Some(0),
                max_capacity_mb: Some(1000),
            },
            policies: Policies::default(),
            volume_total: None,
            batch_bytes: 10 * MB,
        }
    }

    fn with_settings(max_mb: Option<u32>, batch: u64) -> Inputs {
        Inputs {
            settings: BinSettings {
                nuke_on_delete: Some(0),
                max_capacity_mb: max_mb,
            },
            batch_bytes: batch,
            ..ok_inputs()
        }
    }

    const NO: Option<Lock> = Some(Lock::NoRecycleBin);
    const FULL: Option<Lock> = Some(Lock::BinFull);
    const LONG: Option<Lock> = Some(Lock::PathTooLong);
    const ODD: Option<Lock> = Some(Lock::UnsupportedPath);

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
            assert_eq!(decide(&i), NO, "drive type {raw}");
        }
    }

    #[test]
    fn nuke_on_delete_blocks() {
        for (nuke, blocks) in [(Some(0), false), (None, false), (Some(1), true), (Some(7), true)] {
            let mut i = ok_inputs();
            i.settings.nuke_on_delete = nuke;
            assert_eq!(decide(&i) == NO, blocks, "NukeOnDelete {nuke:?}");
        }
    }

    #[test]
    fn a_batch_over_max_capacity_blocks() {
        // 90% of 1000 MB is 900 MB.
        assert_eq!(decide(&with_settings(Some(1000), 900 * MB)), None);
        assert_eq!(decide(&with_settings(Some(1000), 900 * MB + 1)), NO);
        assert_eq!(decide(&with_settings(Some(1000), 2000 * MB)), NO);
        assert_eq!(decide(&with_settings(Some(1000), 1000 * MB)), NO);
        // Unreadable or zero capacity refuses, even for an empty batch.
        assert_eq!(decide(&with_settings(None, 0)), NO);
        assert_eq!(decide(&with_settings(Some(0), 0)), NO);
        // Huge values do not overflow.
        assert_eq!(decide(&with_settings(Some(u32::MAX), u64::MAX)), NO);
    }

    #[test]
    fn what_is_already_in_the_bin_counts() {
        let used = |bin_used| Inputs {
            bin_used,
            ..with_settings(Some(1000), 100 * MB)
        };
        assert_eq!(decide(&used(Some(800 * MB))), None); // 900 MB in all, exactly the margin
        assert_eq!(decide(&used(Some(800 * MB + 1))), FULL);
        assert_eq!(decide(&used(Some(5000 * MB))), FULL);
        assert_eq!(decide(&used(None)), FULL, "a failed bin query blocks");
    }

    #[test]
    fn a_too_long_path_blocks_with_its_own_reason() {
        let long = |top, name, suffix| Inputs {
            top_units: top,
            name_units: name,
            inner_suffix: suffix,
            ..ok_inputs()
        };
        assert_eq!(decide(&long(MAX_ORIGINAL_PATH_UNITS, 10, None)), None);
        assert_eq!(decide(&long(MAX_ORIGINAL_PATH_UNITS + 1, 10, None)), LONG);
        // A file with a long name: the bin prefix counts.
        assert_eq!(decide(&long(100, 179, None)), None);
        assert_eq!(decide(&long(100, 180, None)), LONG);
    }

    #[test]
    fn a_long_path_inside_a_folder_blocks() {
        let inner = |top, name, suffix| path_too_long(top, name, Some(suffix));
        // Full length: top + 1 + suffix.
        assert!(!inner(250, 10, 8)); // 250 + 9 = 259
        assert!(inner(250, 10, 9)); // 260
        // Relative to the selected folder: 80 + name + 1 + suffix.
        assert!(!inner(30, 10, 168)); // 80 + 10 + 169 = 259
        assert!(inner(30, 10, 169)); // 260, though the full path is only 200
        // The plain case the review found: a short folder with a 241-unit inner path.
        assert!(inner(40, 10, 241));
        assert!(!path_too_long(40, 10, None));
        // Through `decide`: an inner path too long for the bin is its own lock.
        let i = Inputs {
            inner_suffix: Some(241),
            ..ok_inputs()
        };
        assert_eq!(decide(&i), LONG);
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
    fn a_folder_mounted_volume_or_a_different_root_blocks() {
        let i = Inputs {
            volume_root_ok: false,
            ..ok_inputs()
        };
        assert_eq!(decide(&i), ODD);
        let i = Inputs {
            same_root: false,
            ..ok_inputs()
        };
        assert_eq!(decide(&i), ODD);
        assert!(same_drive(r"X:\a\b", r"x:\other\b"));
        assert!(!same_drive(r"X:\a", r"d:\a"));
        assert!(!same_drive(r"\\server\share", r"c:\a"));
        assert!(!same_drive(r"c:\a", ""));
    }

    #[test]
    fn only_ntfs_and_refs_pass() {
        for (name, ok) in [("NTFS", true), ("ntfs", true), ("ReFS", true), ("FAT32", false), ("exFAT", false), ("", false), ("CDFS", false)] {
            assert_eq!(fs_allows(name), ok, "{name}");
        }
        let i = Inputs {
            fs_ok: false,
            ..ok_inputs()
        };
        assert_eq!(decide(&i), NO);
    }

    #[test]
    fn policies_block_or_shrink_the_capacity() {
        let with = |p: Policies, total: Option<u64>, batch| Inputs {
            policies: p,
            volume_total: total,
            batch_bytes: batch,
            ..ok_inputs()
        };
        let pol = |nrf, size| Policies {
            no_recycle_files: nrf,
            recycle_bin_size: size,
        };
        use PolicyValue::*;
        assert_eq!(decide(&with(pol(Value(0), Absent), None, MB)), None);
        assert_eq!(decide(&with(pol(Value(1), Absent), None, MB)), NO);
        assert_eq!(decide(&with(pol(Unreadable, Absent), None, MB)), NO);
        assert_eq!(decide(&with(pol(Absent, Unreadable), None, MB)), NO);
        // 10% of a 2000 MB volume is 200 MB, below MaxCapacity 1000 MB: 90% of it is 180 MB.
        let ten = pol(Absent, Value(10));
        assert_eq!(decide(&with(ten, Some(2000 * MB), 180 * MB)), None);
        assert_eq!(decide(&with(ten, Some(2000 * MB), 181 * MB)), NO);
        // A policy percent above MaxCapacity changes nothing.
        assert_eq!(decide(&with(pol(Absent, Value(100)), Some(2000 * MB), 900 * MB)), None);
        // The policy is present but the volume size could not be read.
        assert_eq!(decide(&with(ten, None, MB)), NO);
        // A zero percent leaves no room.
        assert_eq!(decide(&with(pol(Absent, Value(0)), Some(2000 * MB), 0)), NO);
    }

    #[test]
    fn policy_values_merge_over_both_hives() {
        use PolicyValue::*;
        assert_eq!(Absent.merge(Absent, u32::max), Absent);
        assert_eq!(Absent.merge(Value(3), u32::max), Value(3));
        assert_eq!(Value(0).merge(Value(1), u32::max), Value(1));
        assert_eq!(Value(30).merge(Value(10), u32::min), Value(10));
        assert_eq!(Value(30).merge(Unreadable, u32::min), Unreadable);
        assert_eq!(Unreadable.merge(Absent, u32::max), Unreadable);
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

    #[test]
    fn the_walk_reports_the_longest_inner_path() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("f");
        assert_eq!(longest_inner_suffix(&f, false), Ok(None));
        std::fs::create_dir_all(f.join("sub").join("deeper")).unwrap();
        std::fs::write(f.join("a.txt"), b"1").unwrap();
        // `sub\deeper` is 10 units and empty so far.
        assert_eq!(longest_inner_suffix(&f, true), Ok(Some(10)));
        std::fs::write(f.join("sub").join("deeper").join("x.bin"), b"1").unwrap();
        assert_eq!(longest_inner_suffix(&f, true), Ok(Some(16))); // sub\deeper\x.bin
        assert_eq!(longest_inner_suffix(&d.path().join("none"), true), Err(()));
        let empty = d.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert_eq!(longest_inner_suffix(&empty, true), Ok(None));
    }

    /// Depends on this PC's registry, bin level and drive format, so it runs on purpose only:
    /// `cargo test -p seek-clean --lib -- --ignored a_normal_temp_file`.
    #[test]
    #[ignore]
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
