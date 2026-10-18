//! Column-oriented in-memory index of one device.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use crate::model::{Change, Checkpoint, RawEntry, ATTR_DIRECTORY};

pub const NO_PARENT: u32 = u32::MAX;

/// Guard against corrupt (cyclic) parent chains when building paths.
const MAX_DEPTH: usize = 4096;

/// Cheap multiplicative hasher for already well-distributed `u64` ids.
#[derive(Default, Clone, Copy)]
pub struct IdHasher(u64);

impl Hasher for IdHasher {
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0.rotate_left(5) ^ u64::from(*b)).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 32)
    }
}

pub(crate) type IdMap = HashMap<u64, u32, BuildHasherDefault<IdHasher>>;

static NEXT_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_epoch() -> u64 {
    NEXT_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[derive(Clone)]
pub struct DeviceIndex {
    pub(crate) ids: Vec<u64>,
    pub(crate) parents: Vec<u32>,
    pub(crate) name_off: Vec<u32>,
    pub(crate) name_len: Vec<u16>,
    pub(crate) lower_off: Vec<u32>,
    pub(crate) lower_len: Vec<u16>,
    pub(crate) sizes: Vec<u64>,
    pub(crate) modified: Vec<i64>,
    pub(crate) created: Vec<i64>,
    pub(crate) attrs: Vec<u32>,
    pub(crate) alive: Vec<bool>,
    pub(crate) names: String,
    pub(crate) lower: String,
    pub(crate) id_to_row: IdMap,
    pub(crate) root_id: u64,
    /// Bytes in the arenas no longer referenced by any row (renames).
    stale: usize,
    /// Process-unique id of this row numbering; changes whenever rows are renumbered.
    epoch: u64,
    /// Bumped by every `apply`: rows can change in place without a new epoch or row count.
    version: u64,
    pub checkpoint: Option<Checkpoint>,
}

impl std::fmt::Debug for DeviceIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceIndex")
            .field("root_id", &self.root_id)
            .field("rows", &self.len())
            .field("live", &self.live_count())
            .field("checkpoint", &self.checkpoint)
            .finish()
    }
}

fn clamp_len(s: &str) -> usize {
    let mut n = s.len().min(u16::MAX as usize);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    n
}

impl DeviceIndex {
    pub fn empty(root_id: u64) -> Self {
        Self {
            ids: Vec::new(),
            parents: Vec::new(),
            name_off: Vec::new(),
            name_len: Vec::new(),
            lower_off: Vec::new(),
            lower_len: Vec::new(),
            sizes: Vec::new(),
            modified: Vec::new(),
            created: Vec::new(),
            attrs: Vec::new(),
            alive: Vec::new(),
            names: String::new(),
            lower: String::new(),
            id_to_row: IdMap::default(),
            root_id,
            stale: 0,
            epoch: next_epoch(),
            version: 0,
            checkpoint: None,
        }
    }

    pub(crate) fn with_capacity(root_id: u64, rows: usize) -> Self {
        let mut ix = Self::empty(root_id);
        ix.ids.reserve(rows);
        ix.parents.reserve(rows);
        ix.name_off.reserve(rows);
        ix.name_len.reserve(rows);
        ix.lower_off.reserve(rows);
        ix.lower_len.reserve(rows);
        ix.sizes.reserve(rows);
        ix.modified.reserve(rows);
        ix.created.reserve(rows);
        ix.attrs.reserve(rows);
        ix.alive.reserve(rows);
        ix.id_to_row.reserve(rows);
        ix
    }

    /// Assemble from already validated columns (used by `persist::decode_index`).
    /// Returns `None` when ids are not unique.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_columns(
        root_id: u64,
        ids: Vec<u64>,
        parents: Vec<u32>,
        name_off: Vec<u32>,
        name_len: Vec<u16>,
        lower_off: Vec<u32>,
        lower_len: Vec<u16>,
        sizes: Vec<u64>,
        modified: Vec<i64>,
        created: Vec<i64>,
        attrs: Vec<u32>,
        names: String,
        lower: String,
    ) -> Option<Self> {
        let n = ids.len();
        let mut id_to_row = IdMap::default();
        id_to_row.reserve(n);
        for (row, id) in ids.iter().enumerate() {
            if id_to_row.insert(*id, row as u32).is_some() {
                return None;
            }
        }
        Some(Self {
            ids,
            parents,
            name_off,
            name_len,
            lower_off,
            lower_len,
            sizes,
            modified,
            created,
            attrs,
            alive: vec![true; n],
            names,
            lower,
            id_to_row,
            root_id,
            stale: 0,
            epoch: next_epoch(),
            version: 0,
            checkpoint: None,
        })
    }

    /// Append one row; `parent` must already be a row index (or `NO_PARENT`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn push_row(
        &mut self,
        id: u64,
        parent: u32,
        name: &str,
        size: u64,
        modified: i64,
        created: i64,
        attrs: u32,
    ) -> u32 {
        let row = self.ids.len() as u32;
        let n = &name[..clamp_len(name)];
        self.name_off.push(self.names.len() as u32);
        self.name_len.push(n.len() as u16);
        self.names.push_str(n);
        self.lower_off.push(self.lower.len() as u32);
        if n.is_ascii() {
            self.lower_len.push(n.len() as u16);
            self.lower
                .extend(n.bytes().map(|b| b.to_ascii_lowercase() as char));
        } else {
            let l = n.to_lowercase();
            let l = &l[..clamp_len(&l)];
            self.lower_len.push(l.len() as u16);
            self.lower.push_str(l);
        }
        self.ids.push(id);
        self.parents.push(parent);
        self.sizes.push(size);
        self.modified.push(modified);
        self.created.push(created);
        self.attrs.push(attrs);
        self.alive.push(true);
        self.id_to_row.insert(id, row);
        row
    }

    /// Append one entry while building an index top-down (e.g. from a depth-first walk): `parent` is the
    /// row returned for the parent directory, `None` for a top-level entry. Ids must be unique.
    #[allow(clippy::too_many_arguments)]
    pub fn push_child(
        &mut self,
        id: u64,
        parent: Option<u32>,
        name: &str,
        size: u64,
        modified: i64,
        created: i64,
        attrs: u32,
    ) -> u32 {
        debug_assert!(parent.is_none_or(|p| (p as usize) < self.len()));
        self.push_row(id, parent.unwrap_or(NO_PARENT), name, size, modified, created, attrs)
    }

    /// Approximate heap bytes held by this index (columns, arenas, id map).
    pub fn heap_bytes(&self) -> usize {
        use std::mem::size_of;
        let map = self.id_to_row.capacity() * (size_of::<(u64, u32)>() + 1);
        self.ids.capacity() * size_of::<u64>()
            + self.parents.capacity() * size_of::<u32>()
            + self.name_off.capacity() * size_of::<u32>()
            + self.name_len.capacity() * size_of::<u16>()
            + self.lower_off.capacity() * size_of::<u32>()
            + self.lower_len.capacity() * size_of::<u16>()
            + self.sizes.capacity() * size_of::<u64>()
            + self.modified.capacity() * size_of::<i64>()
            + self.created.capacity() * size_of::<i64>()
            + self.attrs.capacity() * size_of::<u32>()
            + self.alive.capacity()
            + self.names.capacity()
            + self.lower.capacity()
            + map
    }

    pub fn from_raw(root_id: u64, entries: Vec<RawEntry>) -> Self {
        let n = entries.len();
        let mut idx = IdMap::default();
        idx.reserve(n);
        for (i, e) in entries.iter().enumerate() {
            if e.id != root_id {
                idx.entry(e.id).or_insert(i as u32);
            }
        }
        // 0 unknown, 1 visiting, 2 reaches root, 3 dropped
        let mut state = vec![0u8; n];
        for (i, e) in entries.iter().enumerate() {
            if e.id == root_id || idx.get(&e.id) != Some(&(i as u32)) {
                state[i] = 3;
            }
        }
        let mut stack: Vec<usize> = Vec::new();
        for start in 0..n {
            if state[start] != 0 {
                continue;
            }
            stack.clear();
            let mut cur = start;
            let result = loop {
                match state[cur] {
                    2 => break 2,
                    3 | 1 => break 3,
                    _ => {}
                }
                state[cur] = 1;
                stack.push(cur);
                let p = entries[cur].parent_id;
                if p == root_id {
                    break 2;
                }
                match idx.get(&p) {
                    Some(&pi) => cur = pi as usize,
                    None => break 3,
                }
            };
            for &s in &stack {
                state[s] = result;
            }
        }
        let mut row_of_entry = vec![NO_PARENT; n];
        let mut rows = 0u32;
        for i in 0..n {
            if state[i] == 2 {
                row_of_entry[i] = rows;
                rows += 1;
            }
        }
        let mut ix = Self::with_capacity(root_id, rows as usize);
        // Consume the entries so each name is freed as soon as it is copied into the arena.
        for (i, e) in entries.into_iter().enumerate() {
            if state[i] != 2 {
                continue;
            }
            let parent = if e.parent_id == root_id {
                NO_PARENT
            } else {
                row_of_entry[idx[&e.parent_id] as usize]
            };
            ix.push_row(
                e.id, parent, &e.name, e.size, e.modified, e.created, e.attrs,
            );
        }
        ix
    }

    /// Identifies the current row numbering: row numbers from one epoch are meaningless in another.
    /// Appending rows keeps the epoch; compaction and rebuilding change it.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Changes on every `apply` (in-place updates keep the epoch and row count).
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn root_id(&self) -> u64 {
        self.root_id
    }

    /// Rows including dead ones.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn live_count(&self) -> usize {
        self.id_to_row.len()
    }

    pub fn is_alive(&self, row: u32) -> bool {
        self.alive[row as usize]
    }

    pub fn id(&self, row: u32) -> u64 {
        self.ids[row as usize]
    }

    pub fn row_of(&self, id: u64) -> Option<u32> {
        self.id_to_row.get(&id).copied()
    }

    pub fn name(&self, row: u32) -> &str {
        let r = row as usize;
        let o = self.name_off[r] as usize;
        &self.names[o..o + self.name_len[r] as usize]
    }

    pub fn name_lower(&self, row: u32) -> &str {
        let r = row as usize;
        let o = self.lower_off[r] as usize;
        &self.lower[o..o + self.lower_len[r] as usize]
    }

    pub fn parent(&self, row: u32) -> Option<u32> {
        match self.parents[row as usize] {
            NO_PARENT => None,
            p => Some(p),
        }
    }

    pub fn size(&self, row: u32) -> u64 {
        self.sizes[row as usize]
    }

    pub fn modified(&self, row: u32) -> i64 {
        self.modified[row as usize]
    }

    pub fn created(&self, row: u32) -> i64 {
        self.created[row as usize]
    }

    pub fn attrs(&self, row: u32) -> u32 {
        self.attrs[row as usize]
    }

    pub fn is_dir(&self, row: u32) -> bool {
        self.attrs[row as usize] & ATTR_DIRECTORY != 0
    }

    /// `"C:\\Users\\me"`; a top-level entry yields the mount unchanged (`"C:\\"`).
    pub fn folder_path(&self, row: u32, mount: &str) -> String {
        let mut parts: Vec<&str> = Vec::new();
        let mut cur = self.parents[row as usize];
        while cur != NO_PARENT && parts.len() < MAX_DEPTH {
            parts.push(self.name(cur));
            cur = self.parents[cur as usize];
        }
        if parts.is_empty() {
            return mount.to_string();
        }
        let mut s =
            String::with_capacity(mount.len() + parts.iter().map(|p| p.len() + 1).sum::<usize>());
        s.push_str(mount.trim_end_matches('\\'));
        for p in parts.iter().rev() {
            s.push('\\');
            s.push_str(p);
        }
        s
    }

    pub fn full_path(&self, row: u32, mount: &str) -> String {
        let mut s = self.folder_path(row, mount);
        if !s.ends_with('\\') {
            s.push('\\');
        }
        s.push_str(self.name(row));
        s
    }

    /// Largest id ever used in this index (`root_id` when empty): ids above it are free.
    pub fn max_id(&self) -> u64 {
        self.ids.iter().copied().fold(self.root_id, u64::max)
    }

    /// Finds live folders by full path (as `full_path(row, mount)` builds it), ignoring case and trailing
    /// backslashes. One pass over the folders for all `paths`; the result is parallel to `paths`. The
    /// mount itself is not a row, so it is never found.
    pub fn find_folders(&self, mount: &str, paths: &[&str]) -> Vec<Option<u32>> {
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        let wanted: Vec<String> = paths.iter().map(|p| p.trim_end_matches('\\').to_lowercase()).collect();
        for (i, p) in wanted.iter().enumerate() {
            if let Some((_, last)) = p.rsplit_once('\\') {
                if !last.is_empty() {
                    by_name.entry(last.to_string()).or_default().push(i);
                }
            }
        }
        let mut found = vec![None; paths.len()];
        if by_name.is_empty() {
            return found;
        }
        for row in 0..self.len() as u32 {
            if !self.alive[row as usize] || !self.is_dir(row) {
                continue;
            }
            let Some(candidates) = by_name.get(self.name_lower(row)) else { continue };
            let full = self.full_path(row, mount).to_lowercase();
            for &i in candidates {
                if found[i].is_none() && wanted[i] == full {
                    found[i] = Some(row);
                }
            }
        }
        found
    }

    /// Live direct children of each row in `parents` (`NO_PARENT` = top-level entries), in one pass.
    pub fn children_of(&self, parents: &std::collections::HashSet<u32>) -> HashMap<u32, Vec<u32>> {
        let mut out: HashMap<u32, Vec<u32>> = parents.iter().map(|&p| (p, Vec::new())).collect();
        for (row, p) in self.parents.iter().enumerate() {
            if self.alive[row] {
                if let Some(v) = out.get_mut(p) {
                    v.push(row as u32);
                }
            }
        }
        out
    }

    /// `mask[row]` is true for every row in `roots` and every row below one of them. O(rows), memoised.
    pub fn subtree_mask(&self, roots: &[u32]) -> Vec<bool> {
        const IN: u8 = 1;
        const OUT: u8 = 2;
        let n = self.len();
        let mut state = vec![0u8; n];
        for &r in roots {
            if (r as usize) < n {
                state[r as usize] = IN;
            }
        }
        let mut chain: Vec<u32> = Vec::new();
        for start in 0..n {
            if state[start] != 0 {
                continue;
            }
            chain.clear();
            let mut cur = start as u32;
            let verdict = loop {
                let c = cur as usize;
                if state[c] != 0 {
                    break state[c];
                }
                if chain.len() > MAX_DEPTH {
                    break OUT;
                }
                chain.push(cur);
                match self.parents[c] {
                    NO_PARENT => break OUT,
                    p => cur = p,
                }
            };
            for &r in &chain {
                state[r as usize] = verdict;
            }
        }
        state.into_iter().map(|s| s == IN).collect()
    }

    /// Byte range of the lower-case arena covering rows `start..end`, when their names are laid out in
    /// row order (true after a build/compact; renames break it).
    pub(crate) fn lower_span(&self, start: u32, end: u32) -> Option<(usize, usize)> {
        let (s, e) = (start as usize, end as usize);
        if s >= e {
            return None;
        }
        let (offs, lens) = (&self.lower_off[s..e], &self.lower_len[s..e]);
        let ordered = offs
            .windows(2)
            .zip(lens)
            .all(|(w, l)| w[0] as usize + *l as usize <= w[1] as usize);
        ordered.then(|| {
            (
                offs[0] as usize,
                offs[e - s - 1] as usize + lens[e - s - 1] as usize,
            )
        })
    }

    /// Rows in `start..end` whose lower-case name contains the needle; `lo..hi` is `lower_span(start, end)`.
    pub(crate) fn rows_containing(
        &self,
        finder: &memchr::memmem::Finder<'_>,
        start: u32,
        end: u32,
        lo: usize,
        hi: usize,
    ) -> Vec<u32> {
        let offs = &self.lower_off[start as usize..end as usize];
        let hay = &self.lower.as_bytes()[lo..hi];
        let nlen = finder.needle().len();
        let mut out = Vec::new();
        let mut cur = 0usize;
        while let Some(p) = finder.find(&hay[cur..]) {
            let abs = lo + cur + p;
            let idx = offs.partition_point(|&o| (o as usize) <= abs) - 1;
            let row = start + idx as u32;
            let row_end = offs[idx] as usize + self.lower_len[row as usize] as usize;
            if abs + nlen <= row_end {
                out.push(row);
                cur = row_end - lo; // one hit per row
            } else {
                cur += p + 1; // match straddles two names; a real one may overlap it
            }
        }
        out
    }

    fn kill_row(&mut self, row: u32) {
        let r = row as usize;
        if self.alive[r] {
            self.alive[r] = false;
            self.id_to_row.remove(&self.ids[r]);
        }
    }

    fn update_row(&mut self, row: u32, parent: u32, e: &RawEntry) {
        let r = row as usize;
        let n = &e.name[..clamp_len(&e.name)];
        if self.name(row) != n {
            self.stale += self.name_len[r] as usize + self.lower_len[r] as usize;
            self.name_off[r] = self.names.len() as u32;
            self.name_len[r] = n.len() as u16;
            self.names.push_str(n);
            let l = n.to_lowercase();
            let l = &l[..clamp_len(&l)];
            self.lower_off[r] = self.lower.len() as u32;
            self.lower_len[r] = l.len() as u16;
            self.lower.push_str(l);
        }
        self.parents[r] = parent;
        self.sizes[r] = e.size;
        self.modified[r] = e.modified;
        self.created[r] = e.created;
        self.attrs[r] = e.attrs;
    }

    /// Kill every live row that has a dead ancestor (or sits in a parent cycle). One pass: each row's
    /// verdict is memoised, so the whole sweep is O(rows) regardless of row order or depth.
    fn sweep_orphans(&mut self) {
        const OK: u8 = 1;
        const DEAD: u8 = 2;
        let n = self.ids.len();
        let mut state = vec![0u8; n];
        let mut chain: Vec<u32> = Vec::new();
        for start in 0..n {
            if state[start] != 0 {
                continue;
            }
            chain.clear();
            let mut cur = start as u32;
            let verdict = loop {
                let c = cur as usize;
                if !self.alive[c] {
                    break DEAD;
                }
                if state[c] != 0 {
                    break state[c];
                }
                if chain.len() > MAX_DEPTH {
                    break DEAD;
                }
                chain.push(cur);
                match self.parents[c] {
                    NO_PARENT => break OK,
                    p => cur = p,
                }
            };
            for &r in &chain {
                state[r as usize] = verdict;
                if verdict == DEAD {
                    self.kill_row(r);
                }
            }
            if state[start] == 0 {
                state[start] = DEAD; // the start row itself was dead
            }
        }
    }

    /// Applies a batch of changes. Changes may arrive out of order (e.g. a file before the new folder
    /// that contains it): upserts whose parent is not known yet are retried once the rest of the batch
    /// is in, and only dropped (or, for known entries, removed) when their parent never shows up.
    pub fn apply(&mut self, changes: &[Change]) {
        self.version += 1;
        let mut sweep = false;
        let mut pending: Vec<&RawEntry> = Vec::new();
        for c in changes {
            match c {
                Change::Upsert(e) => {
                    if e.id != self.root_id && !self.upsert(e) {
                        pending.push(e);
                    }
                }
                Change::Delete { id } => {
                    if let Some(row) = self.row_of(*id) {
                        sweep |= self.is_dir(row);
                        self.kill_row(row);
                    }
                }
            }
        }
        while !pending.is_empty() {
            let before = pending.len();
            pending.retain(|e| !self.upsert(e));
            if pending.len() == before {
                break;
            }
        }
        for e in pending {
            if let Some(row) = self.row_of(e.id) {
                // Moved under a folder we do not know (outside the index): it is gone from here.
                sweep |= self.is_dir(row);
                self.kill_row(row);
            }
        }
        if sweep {
            self.sweep_orphans();
        }
    }

    /// Insert or update `e`; `false` when its parent is not (yet) a live row.
    fn upsert(&mut self, e: &RawEntry) -> bool {
        let parent = if e.parent_id == self.root_id {
            NO_PARENT
        } else {
            match self.id_to_row.get(&e.parent_id) {
                Some(&p) => p,
                None => return false,
            }
        };
        match self.row_of(e.id) {
            Some(row) if parent == row => return false,
            Some(row) => self.update_row(row, parent, e),
            None => {
                self.push_row(e.id, parent, &e.name, e.size, e.modified, e.created, e.attrs);
            }
        }
        true
    }

    /// Drop dead rows, rebuild arenas, remap parents. Idempotent.
    pub fn compact(&mut self) {
        if self.live_count() == self.len() && self.stale == 0 {
            return;
        }
        let mut remap = vec![NO_PARENT; self.len()];
        let mut next = 0u32;
        for (r, a) in self.alive.iter().enumerate() {
            if *a {
                remap[r] = next;
                next += 1;
            }
        }
        let mut out = Self::with_capacity(self.root_id, next as usize);
        out.checkpoint = self.checkpoint;
        for r in 0..self.len() {
            if !self.alive[r] {
                continue;
            }
            let p = self.parents[r];
            let parent = if p == NO_PARENT {
                NO_PARENT
            } else {
                remap[p as usize]
            };
            out.push_row(
                self.ids[r],
                parent,
                self.name(r as u32),
                self.sizes[r],
                self.modified[r],
                self.created[r],
                self.attrs[r],
            );
        }
        *self = out;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ATTR_DIRECTORY;

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

    #[test]
    fn builds_and_drops_root_and_orphans() {
        let ix = sample();
        assert_eq!(ix.live_count(), 4);
        assert!(ix.row_of(5).is_none());
        assert!(ix.row_of(14).is_none());
    }

    #[test]
    fn paths() {
        let ix = sample();
        let r = ix.row_of(12).unwrap();
        assert_eq!(ix.full_path(r, "C:\\"), "C:\\Users\\Me\\Report.PDF");
        assert_eq!(ix.folder_path(r, "C:\\"), "C:\\Users\\Me");
        let top = ix.row_of(10).unwrap();
        assert_eq!(ix.folder_path(top, "C:\\"), "C:\\");
        assert_eq!(
            ix.full_path(r, "\\\\nas\\media\\"),
            "\\\\nas\\media\\Users\\Me\\Report.PDF"
        );
    }

    #[test]
    fn unicode_lowercase_arena() {
        let ix = sample();
        let r = ix.row_of(13).unwrap();
        assert_eq!(ix.name(r), "Ünïcödé İ.txt");
        assert_eq!(ix.name_lower(r), "Ünïcödé İ.txt".to_lowercase());
    }

    #[test]
    fn apply_rename_move_delete() {
        let mut ix = sample();
        ix.apply(&[Change::Upsert(e(12, 10, "renamed.pdf", false))]);
        let r = ix.row_of(12).unwrap();
        assert_eq!(ix.full_path(r, "C:\\"), "C:\\Users\\renamed.pdf");
        ix.apply(&[Change::Upsert(e(20, 11, "new.txt", false))]);
        assert_eq!(ix.live_count(), 5);
        ix.apply(&[Change::Delete { id: 11 }]);
        assert!(ix.row_of(11).is_none());
        assert!(ix.row_of(13).is_none() || !ix.is_alive(ix.row_of(13).unwrap()));
        assert_eq!(ix.live_count(), 2);
        let before = ix.epoch();
        ix.compact();
        assert_eq!(ix.len(), 2);
        assert_ne!(ix.epoch(), before, "compaction renumbers rows");
        let after = ix.epoch();
        ix.compact();
        assert_eq!(ix.len(), 2);
        assert_eq!(ix.epoch(), after, "no-op compaction keeps the numbering");
    }

    #[test]
    fn out_of_order_batch_keeps_children_of_new_folder() {
        let mut ix = sample();
        // journal dedup can emit a new folder's files before the folder itself
        ix.apply(&[
            Change::Upsert(e(41, 40, "a.txt", false)),
            Change::Upsert(e(42, 43, "deep.txt", false)),
            Change::Upsert(e(43, 40, "sub", true)),
            Change::Upsert(e(40, 10, "New folder", true)),
        ]);
        let r = ix.row_of(42).unwrap();
        assert_eq!(ix.full_path(r, "C:\\"), r"C:\Users\New folder\sub\deep.txt");
        assert!(ix.row_of(41).is_some());
        // a file moved into a folder that is created later in the same batch
        ix.apply(&[
            Change::Upsert(e(12, 50, "Report.PDF", false)),
            Change::Upsert(e(50, 5, "Archive", true)),
        ]);
        let r = ix.row_of(12).unwrap();
        assert_eq!(ix.full_path(r, "C:\\"), r"C:\Archive\Report.PDF");
    }

    #[test]
    fn deleting_a_folder_removes_its_subtree_in_any_row_order() {
        // children listed before their parents (MFT order)
        let mut ix = DeviceIndex::from_raw(
            5,
            vec![
                e(4, 3, "leaf.txt", false),
                e(3, 2, "c", true),
                e(2, 1, "b", true),
                e(1, 5, "a", true),
                e(9, 5, "keep.txt", false),
            ],
        );
        ix.apply(&[Change::Delete { id: 2 }]);
        assert_eq!(ix.live_count(), 2);
        assert!(ix.row_of(4).is_none() && ix.row_of(3).is_none());
        // deleting a file never touches anything else
        ix.apply(&[Change::Delete { id: 9 }]);
        assert_eq!(ix.live_count(), 1);
    }

    #[test]
    fn folder_lookup_children_and_subtree_mask() {
        let ix = sample();
        let found = ix.find_folders("C:\\", &[r"c:\users\ME\", r"C:\Users", r"C:\", r"C:\Users\Nobody"]);
        assert_eq!(found, vec![ix.row_of(11), ix.row_of(10), None, None]);
        assert_eq!(ix.max_id(), 13, "dropped entries never became rows");

        let me = ix.row_of(11).unwrap();
        let kids = ix.children_of(&[me, NO_PARENT].into_iter().collect());
        let mut mine: Vec<u64> = kids[&me].iter().map(|&r| ix.id(r)).collect();
        mine.sort();
        assert_eq!(mine, vec![12, 13]);
        assert_eq!(kids[&NO_PARENT].iter().map(|&r| ix.id(r)).collect::<Vec<_>>(), vec![10]);

        let mask = ix.subtree_mask(&[me]);
        let hidden: Vec<u64> = (0..ix.len() as u32).filter(|&r| mask[r as usize]).map(|r| ix.id(r)).collect();
        assert_eq!(hidden.len(), 3);
        assert!(!mask[ix.row_of(10).unwrap() as usize]);
    }

    #[test]
    fn cycles_and_unknown_parent_are_handled() {
        let ix = DeviceIndex::from_raw(
            5,
            vec![e(1, 2, "a", true), e(2, 1, "b", true), e(3, 5, "ok", false)],
        );
        assert_eq!(ix.live_count(), 1);
        let mut ix = sample();
        ix.apply(&[Change::Upsert(e(30, 777, "x", false))]);
        assert_eq!(ix.live_count(), 4);
        // moving a known entry under an unknown parent kills it and its subtree
        ix.apply(&[Change::Upsert(e(11, 777, "Me", true))]);
        assert_eq!(ix.live_count(), 1);
    }
}
