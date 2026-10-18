//! USN change-journal follower for NTFS volumes.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;

use seek_core::{Change, Checkpoint, RawEntry};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, OpenFileById, BY_HANDLE_FILE_INFORMATION, FILE_ID_DESCRIPTOR,
    FILE_ID_DESCRIPTOR_0, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V0,
};

use crate::time::filetime_to_unix_ms;
use crate::win::{self, FRN_MASK};

pub const USN_REASON_FILE_DELETE: u32 = 0x200;

const ERROR_JOURNAL_DELETE_IN_PROGRESS: i32 = 1178;
const ERROR_JOURNAL_NOT_ACTIVE: i32 = 1179;
const ERROR_JOURNAL_ENTRY_DELETED: i32 = 1181;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const READ_BUFFER: usize = 64 * 1024;

const U_RECORD_LENGTH: usize = 0;
const U_MAJOR_VERSION: usize = 4;
const U_FRN: usize = 8;
const U_PARENT_FRN: usize = 16;
const U_REASON: usize = 40;
const U_ATTRS: usize = 52;
const U_NAME_LENGTH: usize = 56;
const U_NAME_OFFSET: usize = 58;
const U_MIN_LEN: usize = 60;

#[derive(Debug)]
pub enum FollowError {
    JournalReset,
    AccessDenied,
    Gone,
    Io(io::Error),
}

impl std::fmt::Display for FollowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::JournalReset => f.write_str("USN journal was reset"),
            Self::AccessDenied => f.write_str("access denied"),
            Self::Gone => f.write_str("volume is gone"),
            Self::Io(e) => write!(f, "i/o error: {e}"),
        }
    }
}

impl std::error::Error for FollowError {}

fn map_follow(e: io::Error) -> FollowError {
    match e.raw_os_error() {
        Some(
            ERROR_JOURNAL_DELETE_IN_PROGRESS
            | ERROR_JOURNAL_NOT_ACTIVE
            | ERROR_JOURNAL_ENTRY_DELETED,
        ) => FollowError::JournalReset,
        Some(win::ERROR_ACCESS_DENIED) => FollowError::AccessDenied,
        Some(win::ERROR_NOT_READY | win::ERROR_FILE_NOT_FOUND | win::ERROR_PATH_NOT_FOUND) => {
            FollowError::Gone
        }
        _ => FollowError::Io(e),
    }
}

pub(crate) fn query_journal(file: &File) -> io::Result<USN_JOURNAL_DATA_V0> {
    // SAFETY: zeroed plain C struct, passed with its exact size.
    let mut j: USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
    unsafe {
        win::ioctl(
            file,
            FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            (&mut j as *mut USN_JOURNAL_DATA_V0).cast(),
            std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
        )
    }?;
    Ok(j)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsnRecord {
    /// Full 64-bit file reference (including the sequence number; mask to 48 bits for ids).
    pub frn: u64,
    pub parent_frn: u64,
    pub reason: u32,
    pub attrs: u32,
    pub name: String,
}

/// Parses the output of `FSCTL_READ_USN_JOURNAL`: next USN followed by `USN_RECORD_V2`s.
/// Stops quietly at the first truncated or non-V2 record.
pub fn parse_usn_buffer(buf: &[u8]) -> (i64, Vec<UsnRecord>) {
    let Some(next) = buf.get(..8) else {
        return (0, Vec::new());
    };
    let next_usn = i64::from_le_bytes(next.try_into().unwrap());
    let mut records = Vec::new();
    let mut off = 8;
    while let Some(rec) = buf.get(off..) {
        let u32_at = |o: usize| {
            rec.get(o..o + 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        };
        let u64_at = |o: usize| {
            rec.get(o..o + 8)
                .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
        };
        let u16_at = |o: usize| {
            rec.get(o..o + 2)
                .map(|b| u16::from_le_bytes(b.try_into().unwrap()))
        };
        let Some(len) = u32_at(U_RECORD_LENGTH).map(|l| l as usize) else {
            break;
        };
        if len < U_MIN_LEN || len > rec.len() {
            break;
        }
        let (
            Some(major),
            Some(frn),
            Some(parent),
            Some(reason),
            Some(attrs),
            Some(nlen),
            Some(noff),
        ) = (
            u16_at(U_MAJOR_VERSION),
            u64_at(U_FRN),
            u64_at(U_PARENT_FRN),
            u32_at(U_REASON),
            u32_at(U_ATTRS),
            u16_at(U_NAME_LENGTH),
            u16_at(U_NAME_OFFSET),
        )
        else {
            break;
        };
        let Some(raw) = rec
            .get(noff as usize..noff as usize + nlen as usize)
            .filter(|_| noff as usize + nlen as usize <= len)
        else {
            break;
        };
        if major == 2 {
            let units: Vec<u16> = raw
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            records.push(UsnRecord {
                frn,
                parent_frn: parent,
                reason,
                attrs,
                name: String::from_utf16_lossy(&units),
            });
        }
        off += len;
    }
    (next_usn, records)
}

/// Follows the NTFS change journal from a [`Checkpoint`].
pub struct UsnFollower {
    volume: File,
    journal_id: u64,
    next_usn: i64,
}

impl UsnFollower {
    pub fn open(mount_point: &str, cp: Checkpoint) -> Result<Self, FollowError> {
        let volume = win::open_volume(mount_point).map_err(map_follow)?;
        let j = query_journal(&volume).map_err(map_follow)?;
        if j.UsnJournalID != cp.journal_id || cp.next_usn < j.FirstUsn {
            return Err(FollowError::JournalReset);
        }
        Ok(Self {
            volume,
            journal_id: cp.journal_id,
            next_usn: cp.next_usn,
        })
    }

    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            journal_id: self.journal_id,
            next_usn: self.next_usn,
        }
    }

    /// Non-blocking: returns the changes since the last call, deduplicated by file id.
    pub fn poll(&mut self) -> Result<Vec<Change>, FollowError> {
        let mut records = Vec::new();
        let mut buf = vec![0u8; READ_BUFFER];
        loop {
            let req = READ_USN_JOURNAL_DATA_V1 {
                StartUsn: self.next_usn,
                ReasonMask: 0xFFFF_FFFF,
                ReturnOnlyOnClose: 1,
                Timeout: 0,
                BytesToWaitFor: 0,
                UsnJournalID: self.journal_id,
                MinMajorVersion: 2,
                MaxMajorVersion: 2,
            };
            // SAFETY: request struct and buffer are valid for the lengths given.
            let n = unsafe {
                win::ioctl(
                    &self.volume,
                    FSCTL_READ_USN_JOURNAL,
                    (&req as *const READ_USN_JOURNAL_DATA_V1).cast(),
                    std::mem::size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                )
            }
            .map_err(map_follow)? as usize;
            let (next, mut recs) = parse_usn_buffer(&buf[..n.min(buf.len())]);
            let progressed = next > self.next_usn;
            if progressed {
                self.next_usn = next;
            }
            let empty = recs.is_empty();
            records.append(&mut recs);
            if empty && !progressed || n <= 8 {
                break;
            }
        }
        Ok(self.to_changes(records))
    }

    fn to_changes(&self, records: Vec<UsnRecord>) -> Vec<Change> {
        // Keep the last record per file, in order of last occurrence.
        let mut last: HashMap<u64, usize> = HashMap::new();
        for (i, r) in records.iter().enumerate() {
            last.insert(r.frn & FRN_MASK, i);
        }
        let mut out = Vec::new();
        for (i, r) in records.iter().enumerate() {
            let id = r.frn & FRN_MASK;
            if last[&id] != i {
                continue;
            }
            if r.reason & USN_REASON_FILE_DELETE != 0 {
                out.push(Change::Delete { id });
            } else if let Some(info) = self.file_info(r.frn) {
                let size = if info.dwFileAttributes & seek_core::ATTR_DIRECTORY != 0 {
                    0
                } else {
                    (info.nFileSizeHigh as u64) << 32 | info.nFileSizeLow as u64
                };
                let ft = |t: windows_sys::Win32::Foundation::FILETIME| {
                    filetime_to_unix_ms((t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64)
                };
                out.push(Change::Upsert(RawEntry {
                    id,
                    parent_id: r.parent_frn & FRN_MASK,
                    name: r.name.clone(),
                    size,
                    modified: ft(info.ftLastWriteTime),
                    created: ft(info.ftCreationTime),
                    attrs: info.dwFileAttributes,
                }));
            }
            // else: the file is already gone again; a later record deletes it.
        }
        out
    }

    fn file_info(&self, frn: u64) -> Option<BY_HANDLE_FILE_INFORMATION> {
        let desc = FILE_ID_DESCRIPTOR {
            dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: 0, // FileIdType
            Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: frn as i64 },
        };
        // SAFETY: descriptor is fully initialised; the handle is closed before returning.
        unsafe {
            let h = OpenFileById(
                self.volume.as_raw_handle() as HANDLE,
                &desc,
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                FILE_FLAG_BACKUP_SEMANTICS,
            );
            if h == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
            let ok = GetFileInformationByHandle(h, &mut info);
            CloseHandle(h);
            (ok != 0).then_some(info)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usn_record(frn: u64, parent: u64, reason: u32, attrs: u32, name: &str) -> Vec<u8> {
        let units: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let len = (U_MIN_LEN + units.len()).div_ceil(8) * 8;
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&frn.to_le_bytes());
        r[16..24].copy_from_slice(&parent.to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        r[52..56].copy_from_slice(&attrs.to_le_bytes());
        r[56..58].copy_from_slice(&(units.len() as u16).to_le_bytes());
        r[58..60].copy_from_slice(&60u16.to_le_bytes());
        r[60..60 + units.len()].copy_from_slice(&units);
        r
    }

    #[test]
    fn parses_two_records_and_next_usn() {
        let mut buf = 4242i64.to_le_bytes().to_vec();
        buf.extend(usn_record(
            0x0003_0000_0000_0123,
            5,
            0x100 | 0x8000_0000,
            0x20,
            "new.txt",
        ));
        buf.extend(usn_record(0x77, 0x40, 0x200 | 0x8000_0000, 0x20, "Ünï.md"));
        let (next, recs) = parse_usn_buffer(&buf);
        assert_eq!(next, 4242);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].frn, 0x0003_0000_0000_0123);
        assert_eq!(recs[0].parent_frn, 5);
        assert_eq!(recs[0].reason, 0x8000_0100);
        assert_eq!(recs[0].attrs, 0x20);
        assert_eq!(recs[0].name, "new.txt");
        assert_eq!(
            recs[1].reason & USN_REASON_FILE_DELETE,
            USN_REASON_FILE_DELETE
        );
        assert_eq!(recs[1].name, "Ünï.md");
    }

    #[test]
    fn truncated_buffer_does_not_panic() {
        let mut buf = 7i64.to_le_bytes().to_vec();
        buf.extend(usn_record(1, 5, 0x100, 0, "a.txt"));
        let full = buf.len();
        for cut in 0..full {
            let (_, recs) = parse_usn_buffer(&buf[..cut]);
            assert!(recs.is_empty(), "cut at {cut}");
        }
        assert_eq!(parse_usn_buffer(&buf[..full]).1.len(), 1);
    }

    #[test]
    fn non_elevated_open_is_access_denied() {
        if win::open_volume("C:\\").is_ok() {
            return; // elevated: covered by the ignored test below
        }
        let cp = Checkpoint {
            journal_id: 1,
            next_usn: 0,
        };
        assert!(matches!(
            UsnFollower::open("C:\\", cp),
            Err(FollowError::AccessDenied)
        ));
    }

    #[test]
    #[ignore = "needs an elevated shell"]
    fn follows_new_file_elevated() {
        let r = crate::scan_ntfs("C:\\", &|_| {}).unwrap();
        let cp = r.checkpoint.expect("journal active");
        let mut f = UsnFollower::open("C:\\", cp).unwrap();
        let dir = std::env::temp_dir();
        let path = dir.join("seek-usn-probe.txt");
        std::fs::write(&path, b"hello").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let changes = f.poll().unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(changes.iter().any(
            |c| matches!(c, Change::Upsert(e) if e.name == "seek-usn-probe.txt" && e.size == 5)
        ));
        assert!(f.checkpoint().next_usn > cp.next_usn);
    }
}
