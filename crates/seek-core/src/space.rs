//! Space view: how many bytes every folder holds, computed from the index (no disk access).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use serde::{Deserialize, Serialize};

use crate::index::DeviceIndex;

pub const ATTR_OFFLINE: u32 = 0x1000;
pub const ATTR_RECALL_ON_OPEN: u32 = 0x40000;
pub const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
const CLOUD_ATTRS: u32 = ATTR_OFFLINE | ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS;

/// Bytes and file counts of every folder of one index (plus the drive root), for one user's view.
/// Slots are rows; slot `len` is the drive root.
pub struct FolderTotals {
    bytes: Vec<u64>,
    files: Vec<u32>,
    cloud: Vec<bool>,
    /// (bytes, files) of hidden children (another user's folders) directly inside a slot.
    hidden: HashMap<usize, (u64, u64)>,
    /// Visible live children of slot `s`: `kids[offsets[s]..offsets[s + 1]]`.
    offsets: Vec<u32>,
    kids: Vec<u32>,
    /// Bit per row: live, reachable from the root and not hidden (lets the drive root be scanned in order).
    visible: Vec<u64>,
    epoch: u64,
    version: u64,
    len: usize,
}

impl FolderTotals {
    /// `hidden[row]` rows (another user's folders) are left out of every listing; their bytes still count
    /// in their ancestors' totals and show as one "Other users" item where the hidden part starts.
    pub fn compute(ix: &DeviceIndex, hidden: Option<&[bool]>) -> FolderTotals {
        let n = ix.len();
        let root = n;
        let is_hidden = |r: usize| hidden.is_some_and(|m| m.get(r).copied().unwrap_or(false));
        let parent_slot = |r: usize| ix.parent(r as u32).map_or(root, |p| p as usize);

        // Child lists of every live row whose parent is live (or the root).
        let mut offsets = vec![0u32; n + 2];
        for r in 0..n {
            let p = parent_slot(r);
            if ix.is_alive(r as u32) && (p == root || ix.is_alive(p as u32)) {
                offsets[p + 1] += 1;
            }
        }
        for s in 1..offsets.len() {
            offsets[s] += offsets[s - 1];
        }
        let mut next = offsets.clone();
        let mut kids = vec![0u32; offsets[n + 1] as usize];
        for r in 0..n {
            let p = parent_slot(r);
            if ix.is_alive(r as u32) && (p == root || ix.is_alive(p as u32)) {
                kids[next[p] as usize] = r as u32;
                next[p] += 1;
            }
        }

        // Rows reachable from the root, each after its parent (a corrupt cycle is never reached).
        let mut order = Vec::with_capacity(kids.len());
        let mut seen = vec![false; n];
        let mut stack = vec![root];
        while let Some(s) = stack.pop() {
            for &c in &kids[offsets[s] as usize..offsets[s + 1] as usize] {
                let c = c as usize;
                if !seen[c] {
                    seen[c] = true;
                    order.push(c);
                    stack.push(c);
                }
            }
        }

        let mut bytes = vec![0u64; n + 1];
        let mut files = vec![0u32; n + 1];
        let mut cloud = vec![false; n + 1];
        let mut hidden_parts: HashMap<usize, (u64, u64)> = HashMap::new();
        for &c in order.iter().rev() {
            if !ix.is_dir(c as u32) {
                bytes[c] += ix.size(c as u32);
                files[c] += 1;
                cloud[c] = ix.attrs(c as u32) & CLOUD_ATTRS != 0;
            }
            let p = parent_slot(c);
            bytes[p] += bytes[c];
            files[p] += files[c];
            if !is_hidden(c) {
                cloud[p] |= cloud[c];
            } else if p == root || !is_hidden(p) {
                let h = hidden_parts.entry(p).or_default();
                h.0 += bytes[c];
                h.1 += u64::from(files[c]);
            }
        }

        // Keep only what the caller may see.
        let mut vis_offsets = Vec::with_capacity(n + 2);
        let mut vis_kids = Vec::with_capacity(kids.len());
        let mut visible = vec![0u64; n.div_ceil(64)];
        vis_offsets.push(0u32);
        for s in 0..=n {
            for &c in &kids[offsets[s] as usize..offsets[s + 1] as usize] {
                // Children of a hidden folder are hidden too (the mask covers whole subtrees).
                if seen[c as usize] && !is_hidden(c as usize) {
                    vis_kids.push(c);
                    visible[c as usize / 64] |= 1 << (c % 64);
                }
            }
            vis_offsets.push(vis_kids.len() as u32);
        }

        FolderTotals {
            bytes,
            files,
            cloud,
            hidden: hidden_parts,
            offsets: vis_offsets,
            kids: vis_kids,
            visible,
            epoch: ix.epoch(),
            version: ix.version(),
            len: n,
        }
    }

    /// Still describes `ix` (same row numbering, no changes applied since).
    pub fn is_current(&self, ix: &DeviceIndex) -> bool {
        self.epoch == ix.epoch() && self.version == ix.version() && self.len == ix.len()
    }

    fn slot(&self, folder: Option<u32>) -> usize {
        folder.map_or(self.len, |r| r as usize)
    }

    fn kids(&self, slot: usize) -> &[u32] {
        &self.kids[self.offsets[slot] as usize..self.offsets[slot + 1] as usize]
    }

    /// Bytes below `folder` (`None` = the whole drive).
    pub fn bytes(&self, folder: Option<u32>) -> u64 {
        self.bytes[self.slot(folder)]
    }

    /// Files below `folder` (`None` = the whole drive).
    pub fn files(&self, folder: Option<u32>) -> u64 {
        u64::from(self.files[self.slot(folder)])
    }

    /// Visible files below `slot`: in row order for the drive root, depth first for a folder.
    fn files_below(&self, ix: &DeviceIndex, slot: usize, mut f: impl FnMut(u32)) {
        if slot == self.len {
            for (w, &bits) in self.visible.iter().enumerate() {
                let mut bits = bits;
                while bits != 0 {
                    let r = (w * 64 + bits.trailing_zeros() as usize) as u32;
                    bits &= bits - 1;
                    if !ix.is_dir(r) {
                        f(r);
                    }
                }
            }
            return;
        }
        let mut stack = vec![slot];
        while let Some(s) = stack.pop() {
            for &c in self.kids(s) {
                if ix.is_dir(c) {
                    stack.push(c as usize);
                } else {
                    f(c);
                }
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SpaceKind {
    Folder,
    File,
    /// `count` items too small to show, merged.
    Smaller { count: u64 },
    /// Other users' folders the caller may not see.
    Hidden,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SpaceNode {
    pub name: String,
    pub kind: SpaceKind,
    pub bytes: u64,
    pub files: u64,
    pub modified: i64,
    /// Holds files that are only in the cloud (OneDrive / Google Drive placeholders).
    pub cloud: bool,
    pub children: Vec<SpaceNode>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum TypeCategory {
    Video,
    Pictures,
    Music,
    Documents,
    Archives,
    Apps,
    System,
    Other,
}

const CATEGORIES: [TypeCategory; 8] = [
    TypeCategory::Video,
    TypeCategory::Pictures,
    TypeCategory::Music,
    TypeCategory::Documents,
    TypeCategory::Archives,
    TypeCategory::Apps,
    TypeCategory::System,
    TypeCategory::Other,
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TypeTotal {
    pub category: TypeCategory,
    pub bytes: u64,
    pub files: u64,
}

/// Most items listed directly in a folder; the rest are merged into one "smaller items" node.
pub const MAX_LISTED: usize = 500;
/// Below the first level, items under this share of the requested folder are merged.
pub const MIN_SHARE: f64 = 0.005;

/// `folder` (`None` = drive root) and `depth` levels below it, largest first.
pub fn space_tree(ix: &DeviceIndex, t: &FolderTotals, folder: Option<u32>, depth: u8) -> SpaceNode {
    let s = t.slot(folder);
    let min = (t.bytes[s] as f64 * MIN_SHARE) as u64;
    SpaceNode {
        name: folder.map(|r| ix.name(r).to_string()).unwrap_or_default(),
        kind: SpaceKind::Folder,
        bytes: t.bytes[s],
        files: u64::from(t.files[s]),
        modified: folder.map_or(0, |r| ix.modified(r)),
        cloud: t.cloud[s],
        children: if depth > 0 { children(ix, t, s, 1, depth, min) } else { Vec::new() },
    }
}

fn children(ix: &DeviceIndex, t: &FolderTotals, s: usize, level: u8, depth: u8, min: u64) -> Vec<SpaceNode> {
    let mut items = t.kids(s).to_vec();
    items.sort_unstable_by_key(|&r| (Reverse(t.bytes[r as usize]), r));
    let keep = if level == 1 {
        items.len().min(MAX_LISTED)
    } else {
        items.partition_point(|&r| t.bytes[r as usize] >= min)
    };
    let mut out: Vec<SpaceNode> = items[..keep]
        .iter()
        .map(|&r| {
            let dir = ix.is_dir(r);
            SpaceNode {
                name: ix.name(r).to_string(),
                kind: if dir { SpaceKind::Folder } else { SpaceKind::File },
                bytes: t.bytes[r as usize],
                files: u64::from(t.files[r as usize]),
                modified: ix.modified(r),
                cloud: t.cloud[r as usize],
                children: if dir && level < depth {
                    children(ix, t, r as usize, level + 1, depth, min)
                } else {
                    Vec::new()
                },
            }
        })
        .collect();
    if let Some(&(hidden_bytes, hidden_files)) = t.hidden.get(&s) {
        let at = out.partition_point(|n| n.bytes >= hidden_bytes);
        out.insert(
            at,
            SpaceNode {
                name: "Other users".into(),
                kind: SpaceKind::Hidden,
                bytes: hidden_bytes,
                files: hidden_files,
                modified: 0,
                cloud: false,
                children: Vec::new(),
            },
        );
    }
    let rest = &items[keep..];
    if !rest.is_empty() {
        out.push(SpaceNode {
            name: String::new(),
            kind: SpaceKind::Smaller { count: rest.len() as u64 },
            bytes: rest.iter().map(|&r| t.bytes[r as usize]).sum(),
            files: rest.iter().map(|&r| u64::from(t.files[r as usize])).sum(),
            modified: rest.iter().map(|&r| ix.modified(r)).max().unwrap_or(0),
            cloud: rest.iter().any(|&r| t.cloud[r as usize]),
            children: Vec::new(),
        });
    }
    out
}

/// The `limit` largest files below `folder`, largest first (rows of `ix`).
pub fn largest_files(ix: &DeviceIndex, t: &FolderTotals, folder: Option<u32>, limit: usize) -> Vec<u32> {
    if limit == 0 {
        return Vec::new();
    }
    let mut heap: BinaryHeap<Reverse<(u64, Reverse<u32>)>> = BinaryHeap::with_capacity(limit + 1);
    t.files_below(ix, t.slot(folder), |r| {
        let size = ix.size(r);
        if heap.len() == limit && heap.peek().is_some_and(|Reverse((min, _))| size <= *min) {
            return;
        }
        heap.push(Reverse((size, Reverse(r))));
        if heap.len() > limit {
            heap.pop();
        }
    });
    let mut out: Vec<(u64, u32)> = heap.into_iter().map(|Reverse((size, Reverse(r)))| (size, r)).collect();
    out.sort_unstable_by_key(|&(size, r)| (Reverse(size), r));
    out.into_iter().map(|(_, r)| r).collect()
}

/// Bytes and files below `folder` by kind of file, largest first; empty kinds are left out.
pub fn type_totals(ix: &DeviceIndex, t: &FolderTotals, folder: Option<u32>) -> Vec<TypeTotal> {
    let mut sums = [(0u64, 0u64); CATEGORIES.len()];
    t.files_below(ix, t.slot(folder), |r| {
        let k = type_of(ix.name_lower(r)) as usize;
        sums[k].0 += ix.size(r);
        sums[k].1 += 1;
    });
    let mut out: Vec<TypeTotal> = CATEGORIES
        .iter()
        .zip(sums)
        .filter(|(_, (_, files))| *files > 0)
        .map(|(&category, (bytes, files))| TypeTotal { category, bytes, files })
        .collect();
    out.sort_by_key(|x| (Reverse(x.bytes), x.category));
    out
}

/// Kind of file from its (lowercase) name.
pub fn type_of(name_lower: &str) -> TypeCategory {
    use TypeCategory::*;
    let b = name_lower.as_bytes();
    let Some(dot) = b.iter().rposition(|&c| c == b'.') else { return Other };
    if dot == 0 || b.len() - dot > 12 {
        return Other;
    }
    match &name_lower[dot + 1..] {
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "mpg" | "mpeg" | "m2ts" | "mts"
        | "vob" | "3gp" => Video,
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "tif" | "tiff" | "webp" | "heic" | "heif" | "avif" | "raw"
        | "cr2" | "cr3" | "nef" | "arw" | "dng" | "psd" | "svg" | "ico" => Pictures,
        "mp3" | "flac" | "wav" | "aac" | "m4a" | "ogg" | "opus" | "wma" | "aiff" | "aif" | "mid" | "midi" => {
            Music
        }
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp" | "rtf" | "txt"
        | "md" | "csv" | "epub" | "pages" | "numbers" | "key" => Documents,
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "iso" | "img" | "cab" | "dmg"
        | "wim" | "esd" | "vhd" | "vhdx" | "vmdk" => Archives,
        "exe" | "dll" | "msi" | "msix" | "appx" | "so" | "dylib" | "jar" | "apk" | "pyd" | "node" | "wasm"
        | "lib" | "pdb" | "obj" | "o" | "a" | "class" | "rlib" | "rmeta" | "js" | "ts" | "py" | "rs" | "c"
        | "cpp" | "h" | "cs" | "java" | "go" | "pak" | "asar" => Apps,
        "sys" | "drv" | "mui" | "cat" | "etl" | "evtx" | "log" | "tmp" | "dmp" | "blf" | "regtrans-ms" => System,
        _ => Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Change, RawEntry, ATTR_DIRECTORY};
    use crate::synth::synth_index;

    fn dir(id: u64, parent: u64, name: &str) -> RawEntry {
        RawEntry { id, parent_id: parent, name: name.into(), size: 0, modified: 5, created: 5, attrs: ATTR_DIRECTORY }
    }

    fn file(id: u64, parent: u64, name: &str, size: u64) -> RawEntry {
        RawEntry { id, parent_id: parent, name: name.into(), size, modified: 7, created: 7, attrs: 0 }
    }

    fn row(ix: &DeviceIndex, name: &str) -> u32 {
        (0..ix.len() as u32).find(|&r| ix.is_alive(r) && ix.name(r) == name).unwrap()
    }

    /// Sum of live files whose parent chain reaches `folder` (`None` = all).
    fn brute(ix: &DeviceIndex, folder: Option<u32>) -> (u64, u64) {
        let (mut bytes, mut files) = (0, 0);
        for r in 0..ix.len() as u32 {
            if !ix.is_alive(r) || ix.is_dir(r) {
                continue;
            }
            let mut cur = ix.parent(r);
            let inside = match folder {
                None => true,
                Some(f) => loop {
                    match cur {
                        Some(c) if c == f => break true,
                        Some(c) => cur = ix.parent(c),
                        None => break false,
                    }
                },
            };
            if inside {
                bytes += ix.size(r);
                files += 1;
            }
        }
        (bytes, files)
    }

    fn names(n: &SpaceNode, out: &mut Vec<String>) {
        out.push(n.name.clone());
        for c in &n.children {
            names(c, out);
        }
    }

    fn users() -> DeviceIndex {
        DeviceIndex::from_raw(
            0,
            vec![
                dir(1, 0, "Users"),
                dir(2, 1, "alice"),
                dir(3, 1, "bob"),
                file(4, 2, "notes.txt", 100),
                file(5, 3, "secret.mkv", 5000),
                dir(6, 3, "deep"),
                file(7, 6, "diary.docx", 300),
                file(8, 0, "pagefile.sys", 2000),
            ],
        )
    }

    #[test]
    fn totals_match_brute_force() {
        let ix = synth_index(20_000, 7);
        let t = FolderTotals::compute(&ix, None);
        assert_eq!((t.bytes(None), t.files(None)), brute(&ix, None));
        for r in 0..ix.len() as u32 {
            if ix.is_alive(r) && ix.is_dir(r) {
                assert_eq!((t.bytes(Some(r)), t.files(Some(r))), brute(&ix, Some(r)), "row {r}");
            }
        }
    }

    #[test]
    fn a_folder_moved_under_a_later_row_is_counted_there() {
        let mut ix = DeviceIndex::from_raw(0, vec![dir(1, 0, "a"), file(2, 1, "x.bin", 40)]);
        ix.apply(&[Change::Upsert(dir(3, 0, "b")), Change::Upsert(dir(1, 3, "a"))]);
        assert!(row(&ix, "a") < row(&ix, "b"), "the child row comes first");
        let t = FolderTotals::compute(&ix, None);
        assert_eq!(t.bytes(Some(row(&ix, "b"))), 40);
        assert_eq!(t.bytes(None), 40);
    }

    #[test]
    fn deleted_files_are_not_counted() {
        let mut ix = users();
        ix.apply(&[Change::Delete { id: 5 }]);
        let t = FolderTotals::compute(&ix, None);
        assert_eq!(t.bytes(Some(row(&ix, "bob"))), 300);
        assert_eq!(t.bytes(None), 2400);
    }

    #[test]
    fn hidden_folders_become_one_other_users_item() {
        let ix = users();
        let mask = ix.subtree_mask(&[row(&ix, "bob")]);
        let t = FolderTotals::compute(&ix, Some(&mask));
        assert_eq!(t.bytes(None), 7400, "totals still include bob's files");
        let tree = space_tree(&ix, &t, None, 4);
        let mut all = Vec::new();
        names(&tree, &mut all);
        for secret in ["bob", "secret.mkv", "deep", "diary.docx"] {
            assert!(!all.contains(&secret.to_string()), "{secret} leaked");
        }
        let users = tree.children.iter().find(|c| c.name == "Users").unwrap();
        let other = users.children.iter().find(|c| c.kind == SpaceKind::Hidden).unwrap();
        assert_eq!((other.name.as_str(), other.bytes, other.files), ("Other users", 5300, 2));
        assert_eq!(users.bytes, 5400);
        assert_eq!(largest_files(&ix, &t, None, 10).len(), 2, "only notes.txt and pagefile.sys");
        let typed: u64 = type_totals(&ix, &t, None).iter().map(|x| x.bytes).sum();
        assert_eq!(typed, 2100, "types count visible files only");
    }

    #[test]
    fn tree_is_sorted_and_tiny_items_are_merged_below_the_first_level() {
        let mut e = vec![file(1, 0, "big.bin", 1_000_000), dir(2, 0, "tiny"), file(3, 0, "mid.bin", 50_000)];
        e.extend((0..300).map(|i| file(10 + i, 2, &format!("t{i}"), 1)));
        let ix = DeviceIndex::from_raw(0, e);
        let t = FolderTotals::compute(&ix, None);
        let tree = space_tree(&ix, &t, None, 3);
        let order: Vec<&str> = tree.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(order, ["big.bin", "mid.bin", "tiny"], "first level is never merged");
        assert_eq!(tree.bytes, 1_050_300);
        let tiny = &tree.children[2];
        assert_eq!(tiny.kind, SpaceKind::Folder);
        assert_eq!(tiny.children.len(), 1);
        assert_eq!(tiny.children[0].kind, SpaceKind::Smaller { count: 300 });
        assert_eq!((tiny.children[0].bytes, tiny.children[0].files), (300, 300));
        assert_eq!(tree.children[0].kind, SpaceKind::File);
        assert!(tree.children[0].children.is_empty());
    }

    #[test]
    fn depth_limits_the_levels_returned() {
        let ix = users();
        let t = FolderTotals::compute(&ix, None);
        let one = space_tree(&ix, &t, None, 1);
        assert!(one.children.iter().all(|c| c.children.is_empty()));
        let alice = space_tree(&ix, &t, Some(row(&ix, "alice")), 1);
        assert_eq!((alice.name.as_str(), alice.bytes, alice.children.len()), ("alice", 100, 1));
    }

    #[test]
    fn the_first_level_is_capped() {
        let ix = DeviceIndex::from_raw(0, (1..=600).map(|i| file(i, 0, &format!("f{i}"), i)).collect());
        let t = FolderTotals::compute(&ix, None);
        let tree = space_tree(&ix, &t, None, 1);
        assert_eq!(tree.children.len(), MAX_LISTED + 1);
        assert_eq!(tree.children[0].name, "f600");
        let rest = tree.children.last().unwrap();
        assert_eq!(rest.kind, SpaceKind::Smaller { count: 100 });
        assert_eq!(rest.bytes, (1..=100).sum::<u64>());
    }

    #[test]
    fn cloud_only_files_mark_their_folders() {
        let mut e = vec![dir(1, 0, "OneDrive"), dir(2, 1, "Docs"), dir(3, 0, "Local"), file(5, 3, "b.txt", 1)];
        let mut c = file(4, 2, "a.docx", 10);
        c.attrs = ATTR_RECALL_ON_DATA_ACCESS;
        e.push(c);
        let ix = DeviceIndex::from_raw(0, e);
        let t = FolderTotals::compute(&ix, None);
        let tree = space_tree(&ix, &t, None, 3);
        let od = tree.children.iter().find(|c| c.name == "OneDrive").unwrap();
        assert!(od.cloud && od.children[0].cloud && od.children[0].children[0].cloud);
        assert!(!tree.children.iter().find(|c| c.name == "Local").unwrap().cloud);
    }

    #[test]
    fn largest_files_stay_inside_the_folder() {
        let ix = users();
        let t = FolderTotals::compute(&ix, None);
        let all: Vec<&str> = largest_files(&ix, &t, None, 3).iter().map(|&r| ix.name(r)).collect();
        assert_eq!(all, ["secret.mkv", "pagefile.sys", "diary.docx"]);
        let bob: Vec<&str> = largest_files(&ix, &t, Some(row(&ix, "bob")), 10).iter().map(|&r| ix.name(r)).collect();
        assert_eq!(bob, ["secret.mkv", "diary.docx"]);
    }

    #[test]
    fn type_totals_add_up_to_the_folder() {
        let ix = synth_index(5_000, 3);
        let t = FolderTotals::compute(&ix, None);
        let types = type_totals(&ix, &t, None);
        assert_eq!(types.iter().map(|x| x.bytes).sum::<u64>(), t.bytes(None));
        assert_eq!(types.iter().map(|x| x.files).sum::<u64>(), t.files(None));
        assert!(types.windows(2).all(|w| w[0].bytes >= w[1].bytes));
        assert!(types.iter().all(|x| x.files > 0));
    }

    #[test]
    fn file_kinds_by_extension() {
        use TypeCategory::*;
        for (name, kind) in [
            ("movie.mkv", Video),
            ("clip.mp4", Video),
            ("img_001.heic", Pictures),
            ("song.flac", Music),
            ("report.pdf", Documents),
            ("backup.7z", Archives),
            ("setup.exe", Apps),
            ("core.dll", Apps),
            ("pagefile.sys", System),
            ("readme", Other),
            (".gitignore", Other),
        ] {
            assert_eq!(type_of(name), kind, "{name}");
        }
    }

    #[test]
    fn totals_go_stale_when_the_index_changes() {
        let mut ix = users();
        let t = FolderTotals::compute(&ix, None);
        assert!(t.is_current(&ix));
        ix.apply(&[Change::Upsert(file(4, 2, "notes.txt", 999))]);
        assert!(!t.is_current(&ix), "in-place size change");
        let t = FolderTotals::compute(&ix, None);
        ix.apply(&[Change::Delete { id: 5 }]);
        ix.compact();
        assert!(!t.is_current(&ix), "renumbered rows");
    }
}
