//! Parallel search over one or more devices.

use std::cmp::Ordering;

use rayon::prelude::*;

use crate::index::DeviceIndex;
use crate::matcher::{Matcher, Scratch};
use crate::model::Device;
use crate::query::Expr;
use crate::types::{ResultRow, SortKey};

const CHUNK_ROWS: usize = 64 * 1024;
/// Above this many hits, name ties are ordered by (device, row) instead of by full path.
const PATH_TIE_MAX_HITS: usize = 50_000;

pub struct SearchDevice<'a> {
    pub device: &'a Device,
    pub index: &'a DeviceIndex,
    pub mount: &'a str,
    pub online: bool,
    /// Name of the PC the device belongs to (this PC's name for local devices).
    pub pc: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub dev: u16,
    pub row: u32,
}

/// Returns `None` when `cancelled()` became true before the search finished.
pub fn search(
    devs: &[SearchDevice],
    expr: &Expr,
    sort: SortKey,
    descending: bool,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Option<Vec<Hit>> {
    let matcher = Matcher::compile(expr);
    let prefilter = matcher.prefilter();
    let dev_lower: Vec<String> = devs
        .iter()
        .map(|d| d.device.display_name.to_lowercase())
        .collect();
    let pc_lower: Vec<String> = devs.iter().map(|d| d.pc.to_lowercase()).collect();

    let mut tasks: Vec<(u16, u32, u32)> = Vec::new();
    for (d, sd) in devs.iter().enumerate() {
        let len = sd.index.len();
        let mut start = 0;
        while start < len {
            let end = (start + CHUNK_ROWS).min(len);
            tasks.push((d as u16, start as u32, end as u32));
            start = end;
        }
    }

    let parts: Vec<Option<Vec<Hit>>> = tasks
        .par_iter()
        .map(|&(d, start, end)| {
            if cancelled() {
                return None;
            }
            let sd = &devs[d as usize];
            let mut scratch = Scratch::default();
            scratch.dev_lower.clone_from(&dev_lower[d as usize]);
            scratch.pc_lower.clone_from(&pc_lower[d as usize]);
            let mut hits = Vec::new();
            match prefilter.and_then(|f| sd.index.lower_span(start, end).map(|span| (f, span))) {
                Some((f, (lo, hi))) => {
                    for row in sd.index.rows_containing(f, start, end, lo, hi) {
                        if sd.index.is_alive(row) && matcher.is_match(sd, row, &mut scratch) {
                            hits.push(Hit { dev: d, row });
                        }
                    }
                }
                None => {
                    for row in start..end {
                        if sd.index.is_alive(row) && matcher.is_match(sd, row, &mut scratch) {
                            hits.push(Hit { dev: d, row });
                        }
                    }
                }
            }
            Some(hits)
        })
        .collect();

    let mut hits = Vec::with_capacity(parts.iter().map(|p| p.as_ref().map_or(0, Vec::len)).sum());
    for p in parts {
        hits.extend(p?);
    }
    if cancelled() {
        return None;
    }

    sort_hits(devs, &mut hits, sort);
    if descending {
        hits.reverse();
    }
    Some(hits)
}

/// First 16 bytes big-endian, zero padded: orders like the string itself. A zero low byte means the
/// whole name fits (names never contain NUL), so equal keys imply equal names without touching the arena.
fn prefix_key(s: &str) -> u128 {
    let b = s.as_bytes();
    let mut a = [0u8; 16];
    let n = b.len().min(16);
    a[..n].copy_from_slice(&b[..n]);
    u128::from_be_bytes(a)
}

/// Sort by `primary`, then lowercase name, then full path (computed once per hit, only inside runs of
/// equal keys). Keys are extracted once into a flat array so the comparator never chases the index
/// columns or builds paths.
fn sort_keyed(devs: &[SearchDevice], hits: &mut [Hit], primary: impl Fn(Hit) -> u64 + Sync) {
    let path_ties = hits.len() <= PATH_TIE_MAX_HITS;
    let nl = |h: Hit| devs[h.dev as usize].index.name_lower(h.row);
    let mut keyed: Vec<(u64, u128, Hit)> = hits
        .par_iter()
        .map(|h| (primary(*h), prefix_key(nl(*h)), *h))
        .collect();
    let same_name = |a: &(u64, u128, Hit), b: &(u64, u128, Hit)| {
        a.1 & 0xFF == 0 || nl(a.2) == nl(b.2)
    };
    keyed.par_sort_unstable_by(|a, b| {
        a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then_with(|| {
            if a.1 & 0xFF == 0 {
                Ordering::Equal
            } else {
                nl(a.2).cmp(nl(b.2))
            }
        })
    });
    for (slot, (_, _, h)) in hits.iter_mut().zip(&keyed) {
        *slot = *h;
    }
    // Runs of equal (primary, name): only long names need the arena to confirm.
    let mut bounds: Vec<usize> = Vec::new();
    let mut start = 0;
    for i in 1..=keyed.len() {
        let same = i < keyed.len()
            && keyed[i].0 == keyed[start].0
            && keyed[i].1 == keyed[start].1
            && same_name(&keyed[start], &keyed[i]);
        if !same {
            if i - start > 1 {
                bounds.push(start);
                bounds.push(i);
            }
            start = i;
        }
    }
    drop(keyed);
    let mut runs: Vec<&mut [Hit]> = Vec::new();
    let mut rest: &mut [Hit] = hits;
    let mut offset = 0;
    for w in bounds.chunks_exact(2) {
        let (_, tail) = rest.split_at_mut(w[0] - offset);
        let (run, tail) = tail.split_at_mut(w[1] - w[0]);
        runs.push(run);
        rest = tail;
        offset = w[1];
    }
    if !path_ties {
        // Huge result sets: equal names fall back to (device, row) order instead of paying for paths.
        runs.par_iter_mut()
            .for_each(|run| run.sort_unstable_by_key(|h| (h.dev, h.row)));
        return;
    }
    runs.par_iter_mut().for_each(|run| {
        run.sort_by_cached_key(|h| {
            let d = &devs[h.dev as usize];
            (d.index.full_path(h.row, d.mount), h.dev, h.row)
        })
    });
}

/// Order-preserving map of a signed timestamp onto `u64`.
fn signed_key(v: i64) -> u64 {
    (v as u64) ^ (1 << 63)
}

fn sort_hits(devs: &[SearchDevice], hits: &mut [Hit], sort: SortKey) {
    let ix = |h: Hit| devs[h.dev as usize].index;
    match sort {
        SortKey::Name => sort_keyed(devs, hits, |_| 0),
        SortKey::Path => hits.par_sort_by_cached_key(|h| {
            let d = &devs[h.dev as usize];
            (
                d.index.full_path(h.row, d.mount).to_lowercase(),
                h.dev,
                h.row,
            )
        }),
        SortKey::Size => sort_keyed(devs, hits, |h| ix(h).size(h.row)),
        SortKey::Modified => sort_keyed(devs, hits, |h| signed_key(ix(h).modified(h.row))),
        SortKey::Device => {
            // Devices with the same display name share a rank, so their hits interleave by name.
            let rank: Vec<u64> = devs
                .iter()
                .map(|d| {
                    let n = &d.device.display_name;
                    devs.iter().filter(|o| &o.device.display_name < n).count() as u64
                })
                .collect();
            sort_keyed(devs, hits, |h| rank[h.dev as usize])
        }
    }
}

pub fn materialize(devs: &[SearchDevice], hit: Hit) -> ResultRow {
    let sd = &devs[hit.dev as usize];
    let ix = sd.index;
    let row = hit.row;
    let folder = ix.folder_path(row, sd.mount);
    let mut full_path = folder.clone();
    if !full_path.ends_with('\\') {
        full_path.push('\\');
    }
    full_path.push_str(ix.name(row));
    ResultRow {
        name: ix.name(row).to_string(),
        folder,
        full_path,
        size: ix.size(row),
        modified: ix.modified(row),
        created: ix.created(row),
        is_dir: ix.is_dir(row),
        attrs: ix.attrs(row),
        device_id: sd.device.id,
        device_name: sd.device.display_name.clone(),
        online: sd.online,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use crate::query::parse_at;
    use crate::synth::synth_index;

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

    fn dev(name: &str, mount: &str) -> Device {
        Device {
            id: uuid::Uuid::new_v4(),
            identity: DeviceIdentity::Volume {
                serial: 1,
                filesystem: "NTFS".into(),
            },
            display_name: name.into(),
            label: String::new(),
            filesystem: "NTFS".into(),
            kind: DeviceKind::Fixed,
            total_bytes: 0,
            free_bytes: 0,
            last_mount_point: mount.into(),
            indexed_by_host: uuid::Uuid::nil(),
            first_seen: 0,
            last_seen: 0,
            index_updated_at: 0,
            state: DeviceState::Online,
            entry_count: 0,
            error: None,
        }
    }

    struct Fixture {
        dc: Device,
        dd: Device,
        ic: DeviceIndex,
        id_: DeviceIndex,
    }

    fn fixture() -> Fixture {
        let ic = DeviceIndex::from_raw(
            5,
            vec![
                e(5, 5, ".", true),
                e(10, 5, "Users", true),
                e(11, 10, "Me", true),
                e(12, 11, "Report.PDF", false),
                e(13, 11, "Ünïcödé İ.txt", false),
            ],
        );
        // sizes <= 100 so `size:>100` only hits device C
        let mut id_ = DeviceIndex::from_raw(
            5,
            vec![
                e(2, 5, "Photos", true),
                e(3, 2, "IMG_001.JPG", false),
                e(4, 5, "report final.docx", false),
            ],
        );
        id_.sizes.iter_mut().for_each(|s| *s = (*s).min(100));
        Fixture {
            dc: dev("System", "C:\\"),
            dd: dev("Backup", "E:\\"),
            ic,
            id_,
        }
    }

    fn devs(f: &Fixture) -> Vec<SearchDevice<'_>> {
        vec![
            SearchDevice {
                device: &f.dc,
                index: &f.ic,
                mount: "C:\\",
                online: true,
                pc: "Laptop",
            },
            SearchDevice {
                device: &f.dd,
                index: &f.id_,
                mount: "E:\\",
                online: false,
                pc: "Desk",
            },
        ]
    }

    fn now() -> chrono::DateTime<chrono::FixedOffset> {
        chrono::DateTime::parse_from_rfc3339("2026-10-04T15:00:00+03:00").unwrap()
    }

    fn run_sorted(f: &Fixture, q: &str, sort: SortKey, desc: bool) -> Vec<Hit> {
        search(&devs(f), &parse_at(q, now()).unwrap(), sort, desc, &|| {
            false
        })
        .unwrap()
    }

    fn names(f: &Fixture, hits: &[Hit]) -> Vec<String> {
        let d = devs(f);
        hits.iter().map(|h| materialize(&d, *h).name).collect()
    }

    #[test]
    fn substring_case_insensitive() {
        let f = fixture();
        // Name order is by lowercased name: "report final.docx" < "report.pdf" because ' ' < '.'
        assert_eq!(
            names(&f, &run_sorted(&f, "report", SortKey::Name, false)),
            vec!["report final.docx", "Report.PDF"]
        );
    }

    #[test]
    fn unicode() {
        let f = fixture();
        assert_eq!(
            names(&f, &run_sorted(&f, "ünï", SortKey::Name, false)),
            vec!["Ünïcödé İ.txt"]
        );
        assert_eq!(
            names(&f, &run_sorted(&f, "İ", SortKey::Name, false)),
            vec!["Ünïcödé İ.txt"]
        );
    }

    #[test]
    fn and_not_or() {
        let f = fixture();
        assert_eq!(
            names(&f, &run_sorted(&f, "report !pdf", SortKey::Name, false)),
            vec!["report final.docx"]
        );
        assert_eq!(run_sorted(&f, "img | pdf", SortKey::Name, false).len(), 2);
    }

    #[test]
    fn wildcard_whole_name() {
        let f = fixture();
        assert_eq!(
            names(&f, &run_sorted(&f, "*.jpg", SortKey::Name, false)),
            vec!["IMG_001.JPG"]
        );
        assert!(run_sorted(&f, "*.jp", SortKey::Name, false).is_empty());
        assert_eq!(
            names(&f, &run_sorted(&f, "img_00?.jpg", SortKey::Name, false)),
            vec!["IMG_001.JPG"]
        );
        assert_eq!(
            names(&f, &run_sorted(&f, "rep*fin*.docx", SortKey::Name, false)),
            vec!["report final.docx"]
        );
    }

    #[test]
    fn filters() {
        let f = fixture();
        assert!(names(&f, &run_sorted(&f, "folder:", SortKey::Name, false))
            .iter()
            .all(|n| ["Users", "Me", "Photos"].contains(&n.as_str())));
        assert_eq!(
            names(
                &f,
                &run_sorted(&f, "offline: ext:docx", SortKey::Name, false)
            ),
            vec!["report final.docx"]
        );
        assert_eq!(run_sorted(&f, "size:>100", SortKey::Name, false).len(), 2);
        assert_eq!(
            run_sorted(&f, "device:backup file:", SortKey::Name, false).len(),
            2
        );
        assert_eq!(
            run_sorted(&f, "online: ext:pdf;txt", SortKey::Name, false).len(),
            2
        );
        assert_eq!(
            run_sorted(&f, "regex:^img_\\d+", SortKey::Name, false).len(),
            1
        );
    }

    #[test]
    fn path_term() {
        let f = fixture();
        assert_eq!(
            names(
                &f,
                &run_sorted(&f, "path:users\\me report", SortKey::Name, false)
            ),
            vec!["Report.PDF"]
        );
        assert_eq!(
            names(
                &f,
                &run_sorted(&f, "path:users/me/report", SortKey::Name, false)
            ),
            vec!["Report.PDF"]
        );
    }

    #[test]
    fn sort_desc_by_size() {
        let f = fixture();
        let d = devs(&f);
        let hits = run_sorted(&f, "file:", SortKey::Size, true);
        let sizes: Vec<u64> = hits.iter().map(|h| materialize(&d, *h).size).collect();
        let mut expect = sizes.clone();
        expect.sort_by(|a, b| b.cmp(a));
        assert_eq!(sizes, expect);
        assert_eq!(sizes[0], 130);
    }

    #[test]
    fn equal_keys_tie_break_by_name() {
        let f = fixture();
        // every folder has size 0 and the same mtime: ties must come out in name order
        for sort in [SortKey::Size, SortKey::Modified] {
            assert_eq!(
                names(&f, &run_sorted(&f, "folder:", sort, false)),
                vec!["Me", "Photos", "Users"]
            );
        }
        let by_dev = run_sorted(&f, "", SortKey::Device, false);
        let d = devs(&f);
        let first: Vec<String> = by_dev.iter().take(3).map(|h| materialize(&d, *h).name).collect();
        assert_eq!(first, vec!["IMG_001.JPG", "Photos", "report final.docx"]);
    }

    #[test]
    fn sort_by_path_and_device() {
        let f = fixture();
        let by_path = run_sorted(&f, "file:", SortKey::Path, false);
        assert_eq!(names(&f, &by_path)[0], "Report.PDF"); // c:\... < e:\...
        let by_dev = run_sorted(&f, "file:", SortKey::Device, false);
        assert_eq!(
            devs(&f)[by_dev[0].dev as usize].device.display_name,
            "Backup"
        );
    }

    fn total_live(f: &Fixture) -> usize {
        f.ic.live_count() + f.id_.live_count()
    }

    #[test]
    fn empty_query_all_live() {
        let f = fixture();
        assert_eq!(
            run_sorted(&f, "", SortKey::Name, false).len(),
            total_live(&f)
        );
    }

    #[test]
    fn cancel_returns_none() {
        let f = fixture();
        let e = parse_at("", now()).unwrap();
        assert!(search(&devs(&f), &e, SortKey::Name, false, &|| true).is_none());
    }

    #[test]
    fn materialize_offline_row() {
        let f = fixture();
        let hit = run_sorted(&f, "report final", SortKey::Name, false)[0];
        let r = materialize(&devs(&f), hit);
        assert!(!r.online);
        assert_eq!(r.folder, "E:\\");
        assert_eq!(r.device_name, "Backup");
        assert_eq!(r.full_path, "E:\\report final.docx");
    }

    #[test]
    fn synth_is_deterministic() {
        assert_eq!(synth_index(1000, 7).live_count(), 1000);
        let (a, b) = (synth_index(500, 3), synth_index(500, 3));
        assert!((0..500u32).all(|r| a.name(r) == b.name(r) && a.parent(r) == b.parent(r)));
    }

    #[test]
    fn arena_prefilter_matches_brute_force() {
        // "xre" + "port" are adjacent in the arena: the straddling "report" must not count
        let mut raw = vec![
            e(5, 5, ".", true),
            e(10, 5, "xre", false),
            e(11, 5, "port", false),
            e(12, 5, "REPORT", false),
        ];
        raw.push(e(13, 5, "rereport", false));
        let ix = DeviceIndex::from_raw(5, raw);
        let d = dev("S", "C:\\");
        let devs = [SearchDevice {
            device: &d,
            index: &ix,
            mount: "C:\\",
            online: true,
            pc: "",
        }];
        let hits = search(
            &devs,
            &parse_at("report", now()).unwrap(),
            SortKey::Name,
            false,
            &|| false,
        )
        .unwrap();
        let got: Vec<_> = hits.iter().map(|h| materialize(&devs, *h).name).collect();
        assert_eq!(got, vec!["REPORT", "rereport"]);

        let big = synth_index(20_000, 9);
        let devs = [SearchDevice {
            device: &d,
            index: &big,
            mount: "C:\\",
            online: true,
            pc: "",
        }];
        for q in ["report", "ort_1", "a", "-v1"] {
            let hits = search(
                &devs,
                &parse_at(q, now()).unwrap(),
                SortKey::Name,
                false,
                &|| false,
            )
            .unwrap();
            let want = (0..big.len() as u32)
                .filter(|r| big.name_lower(*r).contains(q))
                .count();
            assert_eq!(hits.len(), want, "{q}");
        }
        // after a rename the arena is no longer in row order: fallback path must agree
        let mut big = big;
        big.apply(&[Change::Upsert(e(100, 5, "report-renamed", false))]);
        let devs = [SearchDevice {
            device: &d,
            index: &big,
            mount: "C:\\",
            online: true,
            pc: "",
        }];
        let hits = search(
            &devs,
            &parse_at("report-renamed", now()).unwrap(),
            SortKey::Name,
            false,
            &|| false,
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn pc_filter_and_device_term_matching_the_pc_name() {
        let f = fixture();
        let devs = devs(&f);
        let run = |q: &str| search(&devs, &parse_at(q, now()).unwrap(), SortKey::Name, false, &|| false).unwrap();
        let only_on = |hits: Vec<Hit>, d: u16| !hits.is_empty() && hits.iter().all(|h| h.dev == d);
        assert!(only_on(run("pc:lap"), 0));
        assert!(only_on(run("PC:LAP report"), 0));
        assert!(only_on(run("device:desk"), 1));
        assert!(run("pc:nowhere").is_empty());
        // drive: matches the drive's name only, never the device (PC) it belongs to
        assert!(only_on(run("drive:backup"), 1));
        assert!(run("drive:desk").is_empty());
    }

}
