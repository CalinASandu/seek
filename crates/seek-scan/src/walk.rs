//! Parallel directory walker for non-NTFS volumes, network shares and non-elevated runs.

use std::fs::Metadata;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use jwalk::{ClientState, WalkDirGeneric};
use seek_core::DeviceIndex;

use crate::ntfs::map_io;
use crate::{ScanError, ScanResult};

pub const WALK_ROOT_ID: u64 = 0;
const PROGRESS_EVERY: u64 = 4096;

#[derive(Debug, Clone, Default)]
struct Meta {
    size: u64,
    modified: i64,
    created: i64,
    attrs: u32,
}

#[derive(Debug, Default)]
struct State;

impl ClientState for State {
    type ReadDirState = ();
    type DirEntryState = Option<Meta>;
}

fn unix_ms(t: std::io::Result<SystemTime>) -> i64 {
    match t {
        Ok(t) => match t.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => d.as_millis() as i64,
            Err(e) => -(e.duration().as_millis() as i64),
        },
        Err(_) => 0,
    }
}

fn meta_of(m: &Metadata) -> Meta {
    Meta {
        size: if m.is_dir() { 0 } else { m.len() },
        modified: unix_ms(m.modified()),
        created: unix_ms(m.created()),
        attrs: m.file_attributes(),
    }
}

fn walk_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get().clamp(2, 8)).unwrap_or(4)
}

/// Walks `root` in parallel. Hidden and system files are included; symlinks are not followed.
///
/// jwalk yields entries in strict depth-first order, so the index is built directly while walking:
/// the only per-directory state is the chain of ancestors of the current entry (one slot per depth),
/// never a map of every directory or an intermediate list of entries.
pub fn scan_walk(root: &Path, progress: &(dyn Fn(u64) + Sync)) -> Result<ScanResult, ScanError> {
    std::fs::metadata(root).map_err(map_io)?;

    let walker = WalkDirGeneric::<State>::new(root)
        .skip_hidden(false)
        .follow_links(false)
        // A private pool per walk: with the shared rayon pool, jwalk aborts a walk with an error
        // after 1 s when another walk (or a search) keeps the pool busy.
        .parallelism(jwalk::Parallelism::RayonNewPool(walk_threads()))
        .process_read_dir(|_, _, _, children| {
            // Fetch metadata on the worker threads.
            for child in children.iter_mut().flatten() {
                child.client_state = child.metadata().ok().map(|m| meta_of(&m));
            }
        });

    let mut index = DeviceIndex::empty(WALK_ROOT_ID);
    // ancestors[d] = (path, row) of the directory at depth d + 1 on the current branch; `None` row =
    // a directory that was skipped, so its whole subtree is skipped too.
    let mut ancestors: Vec<(PathBuf, Option<u32>)> = Vec::new();
    let mut skipped = 0u64;
    let mut next_id = 1u64;

    for item in walker {
        let Ok(entry) = item else {
            skipped += 1;
            continue;
        };
        let depth = entry.depth();
        if depth == 0 {
            continue;
        }
        ancestors.truncate(depth - 1);
        let parent = if depth == 1 {
            Some(None)
        } else {
            match ancestors.last() {
                Some((path, row)) if path.as_path() == entry.parent_path() => row.map(Some),
                _ => None, // not on the current branch: never expected from a depth-first walk
            }
        };
        let is_dir = entry.file_type().is_dir();
        let row = match (entry.client_state.as_ref(), parent) {
            (Some(meta), Some(parent)) => {
                let id = next_id;
                next_id += 1;
                Some(index.push_child(
                    id,
                    parent,
                    &entry.file_name().to_string_lossy(),
                    meta.size,
                    meta.modified,
                    meta.created,
                    meta.attrs,
                ))
            }
            _ => {
                skipped += 1;
                None
            }
        };
        if is_dir {
            ancestors.push((entry.path(), row));
        }
        if row.is_some() && index.len() as u64 % PROGRESS_EVERY == 0 {
            progress(index.len() as u64);
        }
    }
    progress(index.len() as u64);
    Ok(ScanResult {
        index,
        checkpoint: None,
        skipped,
    })
}
