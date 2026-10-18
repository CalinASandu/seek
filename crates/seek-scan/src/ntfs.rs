//! Raw MFT scanner: reads `$MFT` straight from the volume (needs administrator rights).

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::os::windows::fs::FileExt;

use seek_core::{Checkpoint, DeviceIndex, RawEntry};
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER};

use crate::mft::{self, ParsedRecord};
use crate::win::{self, FRN_MASK};
use crate::{ScanError, ScanResult};

pub const NTFS_ROOT_ID: u64 = 5;
/// Fixup stride of NTFS FILE records, independent of the physical sector size.
const FIXUP_STRIDE: usize = 512;
const CHUNK_BYTES: u64 = 4 * 1024 * 1024;
const FIRST_USER_RECORD: u64 = 16;

pub(crate) fn map_io(e: io::Error) -> ScanError {
    match e.raw_os_error() {
        Some(win::ERROR_ACCESS_DENIED) => ScanError::AccessDenied,
        Some(win::ERROR_FILE_NOT_FOUND | win::ERROR_PATH_NOT_FOUND | win::ERROR_NOT_READY) => {
            ScanError::NotFound
        }
        _ => ScanError::Io(e),
    }
}

struct Base {
    id: u64,
    name: Option<(u64, String)>,
    created: i64,
    modified: i64,
    size: Option<u64>,
    attrs: u32,
}

#[derive(Default)]
struct Ext {
    name: Option<(u64, String, u8)>,
    size: Option<u64>,
}

fn read_exact_at(file: &File, buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], offset)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        done += n;
        offset += n as u64;
    }
    Ok(())
}

/// Scans an NTFS volume (e.g. `"C:\\"`) by reading the Master File Table directly.
pub fn scan_ntfs(
    mount_point: &str,
    progress: &(dyn Fn(u64) + Sync),
) -> Result<ScanResult, ScanError> {
    let file = win::open_volume(mount_point).map_err(map_io)?;

    // SAFETY: `vd` is a plain C struct valid for its own size.
    let mut vd: NTFS_VOLUME_DATA_BUFFER = unsafe { std::mem::zeroed() };
    unsafe {
        win::ioctl(
            &file,
            FSCTL_GET_NTFS_VOLUME_DATA,
            std::ptr::null(),
            0,
            (&mut vd as *mut NTFS_VOLUME_DATA_BUFFER).cast(),
            std::mem::size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
        )
    }
    .map_err(map_io)?;

    let rec_size = vd.BytesPerFileRecordSegment as usize;
    let cluster = vd.BytesPerCluster as u64;
    if !(1024..=64 * 1024).contains(&rec_size) || cluster == 0 || vd.MftStartLcn < 0 {
        return Err(ScanError::Corrupt("implausible NTFS volume data".into()));
    }

    // Query the journal before reading the MFT so changes made during the scan are replayed.
    let checkpoint = crate::usn::query_journal(&file).ok().map(|j| Checkpoint {
        journal_id: j.UsnJournalID,
        next_usn: j.NextUsn,
    });

    // Record 0 describes the MFT's own extents.
    let mut rec0 = vec![0u8; rec_size];
    read_exact_at(&file, &mut rec0, vd.MftStartLcn as u64 * cluster).map_err(map_io)?;
    mft::apply_fixup(&mut rec0, FIXUP_STRIDE)?;
    let runs = mft::parse_record(0, &rec0)?
        .data_runs
        .filter(|r| !r.is_empty())
        .ok_or_else(|| ScanError::Corrupt("$MFT has no data runs".into()))?;

    let mut valid = vd.MftValidDataLength.max(0) as u64;
    valid -= valid % rec_size as u64;

    let mut bases: Vec<Base> = Vec::new();
    let mut exts: HashMap<u64, Ext> = HashMap::new();
    let mut skipped = 0u64;
    let mut buf = vec![0u8; CHUNK_BYTES as usize];
    let mut mft_off = 0u64;
    let mut records_done = 0u64;

    'extents: for (lcn, clusters) in runs {
        let mut ext_off = 0u64;
        let ext_len = clusters * cluster;
        while ext_off < ext_len {
            if mft_off >= valid {
                break 'extents;
            }
            let n = CHUNK_BYTES.min(ext_len - ext_off).min(valid - mft_off) as usize;
            read_exact_at(&file, &mut buf[..n], lcn as u64 * cluster + ext_off).map_err(map_io)?;
            for (i, rec) in buf[..n].chunks_exact_mut(rec_size).enumerate() {
                let frn = (mft_off + (i * rec_size) as u64) / rec_size as u64;
                if frn < FIRST_USER_RECORD {
                    continue;
                }
                if &rec[0..4] != mft::FILE_MAGIC
                    || u16::from_le_bytes([rec[mft::REC_FLAGS], rec[mft::REC_FLAGS + 1]])
                        & mft::FLAG_IN_USE
                        == 0
                {
                    continue;
                }
                match mft::apply_fixup(rec, FIXUP_STRIDE).and_then(|()| mft::parse_record(frn, rec))
                {
                    Ok(p) => absorb(p, &mut bases, &mut exts),
                    Err(_) => skipped += 1,
                }
            }
            records_done += (n / rec_size) as u64;
            mft_off += n as u64;
            ext_off += n as u64;
            progress(records_done);
        }
    }

    let entries = assemble(bases, exts, &mut skipped);
    Ok(ScanResult {
        index: DeviceIndex::from_raw(NTFS_ROOT_ID, entries),
        checkpoint,
        skipped,
    })
}

fn absorb(p: ParsedRecord, bases: &mut Vec<Base>, exts: &mut HashMap<u64, Ext>) {
    if !p.in_use {
        return;
    }
    if p.base_frn == 0 {
        bases.push(Base {
            id: p.frn,
            name: p.name.map(|(parent, name, _)| (parent, name)),
            created: p.created,
            modified: p.modified,
            size: p.size,
            attrs: p.attrs,
        });
    } else {
        let e = exts.entry(p.base_frn).or_default();
        if e.size.is_none() {
            e.size = p.size;
        }
        if let Some(n) = p.name {
            if e.name
                .as_ref()
                .is_none_or(|c| c.2 == mft::NS_DOS && n.2 != mft::NS_DOS)
            {
                e.name = Some(n);
            }
        }
    }
}

/// Merges extension records into their bases and drops entries unreachable from the root.
fn assemble(bases: Vec<Base>, mut exts: HashMap<u64, Ext>, skipped: &mut u64) -> Vec<RawEntry> {
    let mut entries: Vec<RawEntry> = Vec::with_capacity(bases.len());
    for b in bases {
        let ext = if exts.is_empty() {
            None
        } else {
            exts.remove(&b.id)
        };
        let name = b.name.or_else(|| {
            ext.as_ref()
                .and_then(|e| e.name.clone())
                .map(|(p, n, _)| (p, n))
        });
        let Some((parent, name)) = name else {
            continue; // unnamed record (system metadata, orphan extension)
        };
        let size = b
            .size
            .or_else(|| ext.as_ref().and_then(|e| e.size))
            .unwrap_or(0);
        entries.push(RawEntry {
            id: b.id & FRN_MASK,
            parent_id: parent,
            name,
            size,
            modified: b.modified,
            created: b.created,
            attrs: b.attrs,
        });
    }

    // Drop children of skipped system records (e.g. `$Extend`) so every parent link resolves.
    loop {
        let ids: HashSet<u64> = entries.iter().map(|e| e.id).collect();
        let before = entries.len();
        entries.retain(|e| e.parent_id == NTFS_ROOT_ID || ids.contains(&e.parent_id));
        let dropped = before - entries.len();
        *skipped += dropped as u64;
        if dropped == 0 {
            break;
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_elevated_scan_is_access_denied() {
        if is_elevated() {
            return; // only meaningful from a normal shell
        }
        assert!(matches!(
            scan_ntfs("C:\\", &|_| {}),
            Err(ScanError::AccessDenied)
        ));
    }

    #[test]
    fn bad_mount_point_is_not_found() {
        assert!(matches!(scan_ntfs("", &|_| {}), Err(ScanError::NotFound)));
    }

    #[test]
    #[ignore = "needs an elevated shell"]
    fn scan_c_drive_elevated() {
        let t = std::time::Instant::now();
        let r = scan_ntfs("C:\\", &|_| {}).unwrap();
        eprintln!(
            "{} entries in {:?}, skipped {}, cp {:?}",
            r.index.live_count(),
            t.elapsed(),
            r.skipped,
            r.checkpoint
        );
        assert!(r.index.live_count() > 10_000);
        assert_eq!(r.index.root_id(), 5);
    }

    fn is_elevated() -> bool {
        win::open_volume("C:\\").is_ok()
    }

    #[test]
    fn assemble_merges_extension_and_drops_orphans() {
        let base = |id, name: Option<(u64, &str)>| Base {
            id,
            name: name.map(|(p, n)| (p, n.to_string())),
            created: 0,
            modified: 0,
            size: None,
            attrs: 0,
        };
        let bases = vec![
            base(20, Some((5, "dir"))),
            base(21, None),
            base(22, Some((11, "sys"))),
            base(23, Some((22, "kid"))),
        ];
        let mut exts = HashMap::new();
        exts.insert(
            21,
            Ext {
                name: Some((20, "late.txt".into(), 1)),
                size: Some(9),
            },
        );
        let mut skipped = 0;
        let out = assemble(bases, exts, &mut skipped);
        let names: Vec<_> = out.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["dir", "late.txt"]);
        assert_eq!(out[1].size, 9);
        assert_eq!(skipped, 2);
    }
}
