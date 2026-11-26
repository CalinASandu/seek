//! Holds the folders above an item while the elevated process recycles it, so none of them can be
//! swapped for a junction between the last look and the move (a junction would send the recycle
//! somewhere else under this process's admin rights).

use std::any::Any;
use std::fs::File;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    GetFinalPathNameByHandleW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED,
};

use crate::engine::Guard;
use crate::paths::{canon, is_inside};

pub const CHANGED_ABOVE: &str = "A folder above this changed while Seek was removing it, so it was left in place.";

// Listing rights, not just attributes: a handle with only attribute access takes no part in the
// sharing rules, and would not stop the folder being renamed.
const ACCESS: u32 = 0x1 | 0x80; // FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES
const SHARE_READ_WRITE: u32 = 0x1 | 0x2; // never DELETE: a held folder can't be renamed or removed
const ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Holds every folder from the first one below a root down to the item's parent.
pub struct ParentChainGuard {
    /// `canon` form.
    roots: Vec<String>,
}

impl ParentChainGuard {
    pub fn new(roots: Vec<String>) -> ParentChainGuard {
        ParentChainGuard { roots }
    }
}

impl Guard for ParentChainGuard {
    fn hold(&self, path: &Path) -> Result<Box<dyn Any + Send>, String> {
        let c = canon(path);
        // Under none of the roots (Seek's own data in ProgramData): nothing to hold.
        let Some(root) = self.roots.iter().find(|r| is_inside(&c, r) && c != r.trim_end_matches('\\')) else {
            return Ok(Box::new(Vec::<File>::new()));
        };
        let root = root.trim_end_matches('\\');
        let segs: Vec<&str> = c[root.len()..].split('\\').filter(|s| !s.is_empty()).collect();
        let mut held = Vec::new();
        for k in 1..segs.len() {
            let expected = format!("{root}\\{}", segs[..k].join("\\"));
            held.push(open_plain_folder(&expected).ok_or(CHANGED_ABOVE)?);
        }
        Ok(Box::new(held))
    }
}

/// Opens the folder without following a link, shared for read and write but not delete, and
/// checks it is a real folder (no reparse point) that really is at `expected` (`canon` form).
fn open_plain_folder(expected: &str) -> Option<File> {
    let f = std::fs::OpenOptions::new()
        .access_mode(ACCESS)
        .share_mode(SHARE_READ_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(expected)
        .ok()?;
    let meta = f.metadata().ok()?;
    use std::os::windows::fs::MetadataExt;
    if meta.file_attributes() & ATTRIBUTE_REPARSE_POINT != 0 || !meta.is_dir() {
        return None;
    }
    (final_path(&f)? == expected).then_some(f)
}

fn final_path(f: &File) -> Option<String> {
    let h = HANDLE(f.as_raw_handle());
    let mut buf = vec![0u16; 1024];
    loop {
        // SAFETY: a valid handle and a valid buffer.
        let n = unsafe { GetFinalPathNameByHandleW(h, &mut buf, FILE_NAME_NORMALIZED) } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            return Some(canon(Path::new(&String::from_utf16_lossy(&buf[..n]))));
        }
        buf = vec![0u16; n + 1];
    }
}
