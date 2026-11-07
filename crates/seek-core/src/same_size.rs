//! Files that share a size: the first stage of the duplicate finder (the app hashes the candidates later).
//! Reads the index only; no disk access.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use crate::index::DeviceIndex;

fn counts(ix: &DeviceIndex, hidden: Option<&[bool]>, mut keep: impl FnMut(u64) -> bool, mut f: impl FnMut(u64, u32)) {
    for r in 0..ix.len() as u32 {
        if !ix.is_alive(r) || ix.is_dir(r) || hidden.is_some_and(|m| m.get(r as usize).copied().unwrap_or(false)) {
            continue;
        }
        let size = ix.size(r);
        if keep(size) {
            f(size, r);
        }
    }
}

/// Every size (at least `min_size`) held by live, visible files, with how many files have it; largest first.
/// A size seen once is included: the caller adds the counts of several drives before deciding.
pub fn sizes_with_files(ix: &DeviceIndex, hidden: Option<&[bool]>, min_size: u64) -> Vec<(u64, u32)> {
    let mut by_size: HashMap<u64, u32> = HashMap::new();
    counts(ix, hidden, |s| s >= min_size, |s, _| *by_size.entry(s).or_default() += 1);
    let mut out: Vec<(u64, u32)> = by_size.into_iter().collect();
    out.sort_unstable_by_key(|&(size, _)| Reverse(size));
    out
}

/// The rows (ascending) of live, visible files whose size is in `sizes`.
pub fn files_of_size(ix: &DeviceIndex, hidden: Option<&[bool]>, sizes: &HashSet<u64>) -> HashMap<u64, Vec<u32>> {
    let mut out: HashMap<u64, Vec<u32>> = HashMap::new();
    counts(ix, hidden, |s| sizes.contains(&s), |s, r| out.entry(s).or_default().push(r));
    out
}

/// How many live, visible files (at least `min_size`) share each size and lowercase name.
pub fn name_size_counts(ix: &DeviceIndex, hidden: Option<&[bool]>, min_size: u64) -> HashMap<(u64, String), u32> {
    let mut out: HashMap<(u64, String), u32> = HashMap::new();
    counts(ix, hidden, |s| s >= min_size, |s, r| *out.entry((s, ix.name_lower(r).to_string())).or_default() += 1);
    out
}

/// The rows (ascending) of live, visible files whose size and lowercase name are in `keys`, by size.
pub fn files_of_name_size(ix: &DeviceIndex, hidden: Option<&[bool]>, keys: &HashSet<(u64, String)>) -> HashMap<u64, Vec<u32>> {
    let sizes: HashSet<u64> = keys.iter().map(|k| k.0).collect();
    let mut out: HashMap<u64, Vec<u32>> = HashMap::new();
    counts(ix, hidden, |s| sizes.contains(&s), |s, r| {
        if keys.contains(&(s, ix.name_lower(r).to_string())) {
            out.entry(s).or_default().push(r);
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Change, RawEntry, ATTR_DIRECTORY};

    fn dir(id: u64, parent: u64, name: &str) -> RawEntry {
        RawEntry { id, parent_id: parent, name: name.into(), size: 0, modified: 5, created: 5, attrs: ATTR_DIRECTORY }
    }

    fn file(id: u64, parent: u64, name: &str, size: u64) -> RawEntry {
        RawEntry { id, parent_id: parent, name: name.into(), size, modified: 7, created: 7, attrs: 0 }
    }

    fn row(ix: &DeviceIndex, name: &str) -> u32 {
        (0..ix.len() as u32).find(|&r| ix.is_alive(r) && ix.name(r) == name).unwrap()
    }

    #[test]
    fn same_size_skips_folders_deleted_and_hidden_rows() {
        let mut ix = DeviceIndex::from_raw(
            0,
            vec![
                dir(1, 0, "a"),
                dir(2, 0, "b"),
                file(3, 1, "keep1.bin", 500),
                file(4, 1, "gone.bin", 500),
                file(5, 2, "hidden.bin", 500),
                file(6, 2, "other.bin", 700),
            ],
        );
        ix.apply(&[Change::Delete { id: 4 }]);
        let mut mask = vec![false; ix.len()];
        mask[row(&ix, "hidden.bin") as usize] = true;
        // folders have size 0 and are never counted, even with a minimum of 0
        assert_eq!(sizes_with_files(&ix, None, 0), [(700, 1), (500, 2)]);
        assert_eq!(sizes_with_files(&ix, Some(&mask), 0), [(700, 1), (500, 1)]);
        let files = files_of_size(&ix, Some(&mask), &HashSet::from([500, 700, 9]));
        assert_eq!(files.len(), 2);
        assert_eq!(files[&500], [row(&ix, "keep1.bin")]);
        assert_eq!(files[&700], [row(&ix, "other.bin")]);
    }

    #[test]
    fn same_size_is_largest_first_and_honours_the_minimum() {
        let ix = DeviceIndex::from_raw(
            0,
            vec![
                file(1, 0, "a", 10),
                file(2, 0, "b", 3000),
                file(3, 0, "c", 10),
                file(4, 0, "d", 200),
                file(5, 0, "e", 3000),
                file(6, 0, "f", 3000),
            ],
        );
        assert_eq!(sizes_with_files(&ix, None, 0), [(3000, 3), (200, 1), (10, 2)]);
        assert_eq!(sizes_with_files(&ix, None, 200), [(3000, 3), (200, 1)], "the minimum is inclusive");
        assert_eq!(sizes_with_files(&ix, None, 3001), []);
        let files = files_of_size(&ix, None, &HashSet::from([3000]));
        assert_eq!(files[&3000], [row(&ix, "b"), row(&ix, "e"), row(&ix, "f")]);
    }

    #[test]
    fn same_name_is_case_insensitive_and_needs_the_same_size() {
        let ix = DeviceIndex::from_raw(
            0,
            vec![
                file(1, 0, "Report.PDF", 2000),
                file(2, 0, "report.pdf", 2000),
                file(3, 0, "report.pdf", 3000),
                file(4, 0, "other.pdf", 2000),
            ],
        );
        let counts = name_size_counts(&ix, None, 0);
        assert_eq!(counts[&(2000, "report.pdf".to_string())], 2);
        assert_eq!(counts[&(3000, "report.pdf".to_string())], 1);
        assert_eq!(counts[&(2000, "other.pdf".to_string())], 1);
        let files = files_of_name_size(&ix, None, &HashSet::from([(2000, "report.pdf".to_string())]));
        assert_eq!(files.len(), 1);
        assert_eq!(files[&2000], [row(&ix, "Report.PDF"), row(&ix, "report.pdf")]);
    }
}
