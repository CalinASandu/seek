//! Parallel directory walker for non-NTFS volumes, network shares and non-elevated runs.

use std::collections::{HashMap, HashSet};
use std::fs::Metadata;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use jwalk::{ClientState, WalkDirGeneric};
use seek_core::{Change, DeviceIndex, RawEntry, ATTR_DIRECTORY, NO_PARENT};

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

/// Brings the folders `dirs` (absolute paths under `mount`) of a walked index up to date with the disk.
///
/// Each folder is listed once (not recursively) and compared with its children in the index: entries that
/// disappeared are deleted, changed ones updated, new files added and new folders walked completely.
/// A folder that is not in the index (e.g. created during the same burst) is replaced by its nearest
/// indexed ancestor, which then sees it as new. Entries are matched by name, ignoring case, since a
/// walk has no stable file ids. Returns the changes for `DeviceIndex::apply`.
pub fn rescan_dirs(index: &DeviceIndex, mount: &str, dirs: &[PathBuf]) -> Result<Vec<Change>, ScanError> {
    let mount_t = mount.trim_end_matches('\\');
    let mount_l = mount_t.to_lowercase();
    std::fs::metadata(format!("{mount_t}\\")).map_err(map_io)?;

    // lower-case path → (path as given, row; NO_PARENT for the mount)
    let mut targets: HashMap<String, (String, u32)> = HashMap::new();
    let mut todo: Vec<String> = Vec::new();
    for d in dirs {
        let p = d.to_string_lossy().trim_end_matches('\\').to_string();
        let l = p.to_lowercase();
        if l == mount_l || l.starts_with(&format!("{mount_l}\\")) {
            todo.push(p);
        }
    }
    let mut seen: HashSet<String> = HashSet::new();
    while !todo.is_empty() {
        todo.retain(|p| seen.insert(p.to_lowercase()));
        let (root, below): (Vec<String>, Vec<String>) = todo.drain(..).partition(|p| p.to_lowercase() == mount_l);
        if !root.is_empty() {
            targets.insert(mount_l.clone(), (mount_t.to_string(), NO_PARENT));
        }
        let refs: Vec<&str> = below.iter().map(String::as_str).collect();
        for (p, row) in below.iter().zip(index.find_folders(mount, &refs)) {
            match row {
                Some(row) => {
                    targets.insert(p.to_lowercase(), (p.clone(), row));
                }
                // Not indexed (yet): its parent lists it, so rescan that instead.
                None => todo.push(p.rsplit_once('\\').map_or(mount_t, |(parent, _)| parent).to_string()),
            }
        }
    }

    let rows: HashSet<u32> = targets.values().map(|&(_, row)| row).collect();
    let children = index.children_of(&rows);
    let mut next_id = index.max_id() + 1;
    let mut changes = Vec::new();
    for (path, row) in targets.into_values() {
        let parent_id = if row == NO_PARENT { index.root_id() } else { index.id(row) };
        let listing = match std::fs::read_dir(format!("{path}\\")) {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if row != NO_PARENT {
                    changes.push(Change::Delete { id: parent_id });
                }
                continue;
            }
            Err(e) => {
                tracing::debug!("rescan of {path} skipped: {e}");
                continue;
            }
        };
        let mut on_disk: HashMap<String, (String, Meta, bool)> = HashMap::new();
        for entry in listing.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            // Only real folders are walked (links are never followed), like `scan_walk`.
            let walk_into = entry.file_type().is_ok_and(|t| t.is_dir());
            on_disk.insert(name.to_lowercase(), (name, meta_of(&meta), walk_into));
        }
        for &child in children.get(&row).into_iter().flatten() {
            let Some((name, meta, walk_into)) = on_disk.remove(index.name_lower(child)) else {
                changes.push(Change::Delete { id: index.id(child) });
                continue;
            };
            let was_dir = index.is_dir(child);
            if was_dir != (meta.attrs & ATTR_DIRECTORY != 0) {
                // A file became a folder or the other way round: a different entry under the same name.
                changes.push(Change::Delete { id: index.id(child) });
                on_disk.insert(name.to_lowercase(), (name, meta, walk_into));
                continue;
            }
            let same = index.name(child) == name
                && index.size(child) == meta.size
                && index.modified(child) == meta.modified
                && index.created(child) == meta.created
                && index.attrs(child) == meta.attrs;
            if !same {
                changes.push(upsert(index.id(child), parent_id, name, &meta));
            }
        }
        for (name, meta, walk_into) in on_disk.into_values() {
            let id = next_id;
            next_id += 1;
            let sub = walk_into.then(|| format!("{path}\\{name}"));
            changes.push(upsert(id, parent_id, name, &meta));
            if let Some(sub) = sub {
                next_id = add_subtree(Path::new(&sub), id, next_id, &mut changes);
            }
        }
    }
    Ok(changes)
}

fn upsert(id: u64, parent_id: u64, name: String, m: &Meta) -> Change {
    Change::Upsert(RawEntry {
        id,
        parent_id,
        name,
        size: m.size,
        modified: m.modified,
        created: m.created,
        attrs: m.attrs,
    })
}

/// Walks a new folder and appends its whole subtree, numbering entries from `base`; returns the next free id.
fn add_subtree(dir: &Path, dir_id: u64, base: u64, changes: &mut Vec<Change>) -> u64 {
    let sub = match scan_walk(dir, &|_| {}) {
        Ok(r) => r.index,
        Err(e) => {
            tracing::debug!("walk of new folder {} failed: {e}", dir.display());
            return base;
        }
    };
    // Walk ids start at 1 and parents come before children, so the batch applies in order.
    for row in 0..sub.len() as u32 {
        let parent_id = sub.parent(row).map_or(dir_id, |p| base + sub.id(p));
        changes.push(Change::Upsert(RawEntry {
            id: base + sub.id(row),
            parent_id,
            name: sub.name(row).to_string(),
            size: sub.size(row),
            modified: sub.modified(row),
            created: sub.created(row),
            attrs: sub.attrs(row),
        }));
    }
    base + sub.max_id() + 1
}
