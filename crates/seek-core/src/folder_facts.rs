//! What a folder holds, read from the index in one pass: size, file count, newest change, whether
//! it has files, subfolders, programs, links or online-only placeholders, and its first names.
//! The Leftovers scan uses it instead of walking the disk. No disk access, no Windows APIs.

use serde::{Deserialize, Serialize};

use crate::index::DeviceIndex;
use crate::model::ATTR_DIRECTORY;
use crate::space::{ATTR_OFFLINE, ATTR_RECALL_ON_DATA_ACCESS, ATTR_RECALL_ON_OPEN};

const ATTR_REPARSE: u32 = 0x400;
const CLOUD_ATTRS: u32 = ATTR_OFFLINE | ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS;
/// How many names directly inside a folder are kept.
pub const TOP_NAMES: usize = 6;
const MAX_CHAIN: usize = 4096;

/// What the index knows about one folder. Links and online-only entries are not counted in the
/// totals (and are not entered): they only set `has_link` / `has_cloud`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderFact {
    pub size: u64,
    pub files: u64,
    /// Newest modification of anything inside, in unix epoch milliseconds. 0 if nothing is inside.
    pub newest_modified: i64,
    /// Anything that is not a folder (a link counts), at any depth.
    pub has_files: bool,
    /// A folder directly inside.
    pub has_subfolders: bool,
    /// A `.exe` or `.dll` anywhere inside.
    pub has_programs: bool,
    pub has_link: bool,
    pub has_cloud: bool,
    /// The first names directly inside, alphabetical, ignoring case.
    pub top_names: Vec<String>,
}

fn is_program(lower_name: &str) -> bool {
    lower_name.ends_with(".exe") || lower_name.ends_with(".dll")
}

fn keep_name(top: &mut Vec<String>, name: &str) {
    let key = name.to_lowercase();
    let at = top.partition_point(|t| t.to_lowercase() <= key);
    if at < TOP_NAMES {
        top.insert(at, name.to_string());
        top.truncate(TOP_NAMES);
    }
}

/// Facts for each folder row in `rows` (parallel to it). One pass over the index however many
/// folders are asked about; a row inside another asked-about row also counts toward the outer one.
///
/// `hidden` is the connection's mask (another user's folders). Hidden rows still count toward the
/// totals, as they do in the space view, but their names are never listed and a hidden folder
/// does not make `has_subfolders` true. Asking about a hidden folder itself is for the caller to refuse.
pub fn folder_facts(ix: &DeviceIndex, hidden: Option<&[bool]>, rows: &[u32]) -> Vec<FolderFact> {
    const UNSET: u32 = u32::MAX;
    const NONE: u32 = u32::MAX - 1;
    let n = ix.len();
    let is_hidden = |r: usize| hidden.is_some_and(|m| m.get(r).copied().unwrap_or(false));
    // Which entry of `rows` a row is (the first, if a row is asked about twice).
    let mut target_of = vec![UNSET; n];
    for (i, &r) in rows.iter().enumerate() {
        if (r as usize) < n && target_of[r as usize] == UNSET {
            target_of[r as usize] = i as u32;
        }
    }
    // Nearest asked-about folder strictly above each row, memoised.
    let mut owner = vec![UNSET; n];
    let mut chain: Vec<usize> = Vec::new();
    let mut resolve = |x: usize, owner: &mut Vec<u32>| -> u32 {
        chain.clear();
        chain.push(x);
        let mut cur = x;
        let verdict = loop {
            let Some(p) = ix.parent(cur as u32).map(|p| p as usize) else { break NONE };
            if p >= n || !ix.is_alive(p as u32) {
                break NONE;
            }
            if target_of[p] != UNSET {
                break target_of[p];
            }
            if owner[p] != UNSET {
                break owner[p];
            }
            if chain.len() > MAX_CHAIN {
                break NONE;
            }
            chain.push(p);
            cur = p;
        };
        for &c in &chain {
            owner[c] = verdict;
        }
        verdict
    };

    let mut facts = vec![FolderFact::default(); rows.len()];
    for r in 0..n {
        if !ix.is_alive(r as u32) {
            continue;
        }
        let o = if owner[r] == UNSET { resolve(r, &mut owner) } else { owner[r] };
        if o == NONE {
            continue;
        }
        let f = &mut facts[o as usize];
        let attrs = ix.attrs(r as u32);
        let direct = ix.parent(r as u32) == Some(rows[o as usize]);
        f.newest_modified = f.newest_modified.max(ix.modified(r as u32));
        if direct && !is_hidden(r) {
            keep_name(&mut f.top_names, ix.name(r as u32));
        }
        if attrs & CLOUD_ATTRS != 0 {
            f.has_cloud = true;
        } else if attrs & ATTR_REPARSE != 0 {
            f.has_link = true;
            f.has_files = true;
        } else if attrs & ATTR_DIRECTORY != 0 {
            f.has_subfolders |= direct && !is_hidden(r);
        } else {
            f.has_files = true;
            f.files += 1;
            f.size = f.size.saturating_add(ix.size(r as u32));
            f.has_programs |= is_program(ix.name_lower(r as u32));
        }
    }

    // A folder asked about inside another counts toward the outer one too, innermost first.
    let depth = |i: usize| {
        let (mut d, mut cur) = (0, i);
        while d < MAX_CHAIN {
            let o = owner[rows[cur] as usize];
            if o >= NONE {
                break;
            }
            d += 1;
            cur = o as usize;
        }
        d
    };
    let mut by_depth: Vec<usize> = (0..rows.len()).filter(|&i| target_of[rows[i] as usize] == i as u32).collect();
    by_depth.sort_by_key(|&i| std::cmp::Reverse(depth(i)));
    for i in by_depth {
        let o = owner[rows[i] as usize];
        if o >= NONE {
            continue;
        }
        let inner = facts[i].clone();
        let outer = &mut facts[o as usize];
        outer.size = outer.size.saturating_add(inner.size);
        outer.files += inner.files;
        outer.newest_modified = outer.newest_modified.max(inner.newest_modified);
        outer.has_files |= inner.has_files;
        outer.has_programs |= inner.has_programs;
        outer.has_link |= inner.has_link;
        outer.has_cloud |= inner.has_cloud;
    }
    // The same row asked twice gets the same answer.
    for i in 0..rows.len() {
        let first = target_of.get(rows[i] as usize).copied().unwrap_or(UNSET);
        if first != UNSET && first as usize != i {
            facts[i] = facts[first as usize].clone();
        }
    }
    facts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Change, RawEntry};

    fn dir(id: u64, parent: u64, name: &str) -> RawEntry {
        RawEntry { id, parent_id: parent, name: name.into(), size: 0, modified: 10, created: 10, attrs: ATTR_DIRECTORY }
    }

    fn file(id: u64, parent: u64, name: &str, size: u64, modified: i64, attrs: u32) -> RawEntry {
        RawEntry { id, parent_id: parent, name: name.into(), size, modified, created: 1, attrs }
    }

    fn row(ix: &DeviceIndex, name: &str) -> u32 {
        (0..ix.len() as u32).find(|&r| ix.is_alive(r) && ix.name(r) == name).unwrap()
    }

    fn index() -> DeviceIndex {
        DeviceIndex::from_raw(
            0,
            vec![
                dir(1, 0, "App"),
                dir(2, 1, "Sub"),
                dir(3, 2, "Deeper"),
                file(4, 3, "deep.txt", 100, 500, 0),
                file(5, 1, "readme.TXT", 20, 300, 0),
                file(6, 2, "tool.EXE", 1000, 900, 0),
                dir(7, 0, "Empty"),
                dir(8, 7, "StillEmpty"),
                dir(9, 0, "Linked"),
                file(10, 9, "jump", 0, 50, ATTR_REPARSE | ATTR_DIRECTORY),
                dir(11, 0, "Cloudy"),
                file(12, 11, "online.doc", 5000, 60, 0x40_0000),
                file(13, 11, "local.doc", 7, 70, 0),
                dir(14, 0, "Mixed"),
                file(15, 14, "zeta", 1, 1, 0),
                file(16, 14, "Alpha", 1, 1, 0),
                file(17, 14, "beta", 1, 1, 0),
            ],
        )
    }

    #[test]
    fn totals_flags_and_names_come_from_one_pass() {
        let ix = index();
        let rows: Vec<u32> = ["App", "Empty", "Linked", "Cloudy", "Mixed"].iter().map(|n| row(&ix, n)).collect();
        let f = folder_facts(&ix, None, &rows);
        let app = &f[0];
        assert_eq!((app.size, app.files), (1120, 3));
        assert_eq!(app.newest_modified, 900);
        assert!(app.has_files && app.has_subfolders && app.has_programs && !app.has_link && !app.has_cloud);
        assert_eq!(app.top_names, vec!["readme.TXT".to_string(), "Sub".to_string()]);
        // Nothing but empty folders.
        let empty = &f[1];
        assert_eq!((empty.size, empty.files, empty.has_files, empty.has_subfolders), (0, 0, false, true));
        assert_eq!(empty.newest_modified, 10);
        // A reparse entry counts as something, is not counted in the size, and is flagged.
        let linked = &f[2];
        assert!(linked.has_link && linked.has_files && !linked.has_subfolders);
        assert_eq!((linked.size, linked.files), (0, 0));
        // An online-only entry is flagged and neither counted nor listed as a file of the totals.
        let cloudy = &f[3];
        assert!(cloudy.has_cloud);
        assert_eq!((cloudy.size, cloudy.files), (7, 1));
        // Names: alphabetical, case ignored.
        assert_eq!(f[4].top_names, vec!["Alpha", "beta", "zeta"]);
    }

    #[test]
    fn only_six_names_are_kept() {
        let mut entries = vec![dir(1, 0, "Big")];
        for i in 0..20u64 {
            entries.push(file(10 + i, 1, &format!("f{i:02}.txt"), 1, 1, 0));
        }
        let ix = DeviceIndex::from_raw(0, entries);
        let f = folder_facts(&ix, None, &[row(&ix, "Big")]);
        assert_eq!(f[0].top_names.len(), TOP_NAMES);
        assert_eq!(f[0].top_names[0], "f00.txt");
        assert_eq!(f[0].files, 20);
    }

    #[test]
    fn hidden_rows_count_but_are_never_named() {
        let ix = index();
        let mut mask = vec![false; ix.len()];
        mask[row(&ix, "Sub") as usize] = true;
        mask[row(&ix, "Deeper") as usize] = true;
        mask[row(&ix, "deep.txt") as usize] = true;
        mask[row(&ix, "tool.EXE") as usize] = true;
        let f = folder_facts(&ix, Some(&mask), &[row(&ix, "App")]);
        assert_eq!(f[0].top_names, vec!["readme.TXT".to_string()]);
        assert!(!f[0].has_subfolders);
        assert_eq!(f[0].size, 1120, "hidden bytes still count, as in the space view");
    }

    #[test]
    fn a_folder_inside_another_counts_toward_both() {
        let ix = index();
        let rows = [row(&ix, "Sub"), row(&ix, "App"), row(&ix, "Sub")];
        let f = folder_facts(&ix, None, &rows);
        assert_eq!((f[0].size, f[0].files), (1100, 2));
        assert_eq!((f[1].size, f[1].files), (1120, 3));
        assert_eq!(f[2], f[0], "the same folder asked twice");
        assert_eq!(f[1].top_names, vec!["readme.TXT".to_string(), "Sub".to_string()]);
    }

    #[test]
    fn deleted_rows_are_not_counted() {
        let mut ix = index();
        ix.apply(&[Change::Delete { id: 6 }]);
        let f = folder_facts(&ix, None, &[row(&ix, "App")]);
        assert_eq!(f[0].size, 120);
        assert!(!f[0].has_programs);
    }
}
