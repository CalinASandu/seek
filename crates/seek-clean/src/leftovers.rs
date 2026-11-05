//! The Leftovers scan: the top-level folders of Program Files and AppData that no installed app,
//! service, shortcut or running program claims, each with a confidence level and the reasons.
//!
//! What it promises:
//! - Reads only metadata. No file is opened, so a cloud placeholder is never hydrated; a folder
//!   with a placeholder (or a link) inside it is left out rather than judged.
//! - Links are not followed, and a top-level link is not listed.
//! - Memory stays small: a walk keeps totals and a handful of names, never a list of files.
//! - One folder never fails the scan. A folder that can't be read, or that vanishes while it is
//!   being read, is left out.
//! - Nothing here removes anything, and no row is called safe.

use std::collections::HashSet;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::claims::InstalledNames;
use crate::confidence::{is_publisher_name, names_are_similar, score, Evidence};
use crate::facts::{is_cloud, modified_ms, ClaimCheck, FileFacts, ATTR_DIRECTORY, ATTR_REPARSE};
use crate::model::LeftoverRow;
use crate::paths::canon;
use crate::protect::{never_listed, Places};

/// A folder with at least this many files "has many files".
pub const MANY_FILES: u64 = 1000;
/// How many names of what is inside a row shows.
pub const CONTENTS_SHOWN: usize = 6;
const DAY_MS: i64 = 86_400_000;
const YEAR_MS: i64 = 365 * DAY_MS;
const MONTH_MS: i64 = 30 * DAY_MS;

/// Folder names that say nothing about whose they are.
const GENERIC_NAMES: &[&str] = &[
    "data", "common", "shared", "bin", "config", "cache", "logs", "tools", "plugins", "settings", "resources", "updates",
];

/// Where the facts of many folders can come from without walking the disk (Seek's own index).
/// One call for all the candidates; `None` for a folder it doesn't know, which is then walked.
pub trait IndexFacts: Sync {
    fn summaries(&self, folders: &[PathBuf]) -> Vec<Option<Summary>>;
}

/// No index to ask: every folder is walked.
pub struct NoIndex;

impl IndexFacts for NoIndex {
    fn summaries(&self, folders: &[PathBuf]) -> Vec<Option<Summary>> {
        vec![None; folders.len()]
    }
}

/// What is known of a folder: from the index or from one walk. Totals and a few names, nothing per file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub size: u64,
    pub files: u64,
    /// Newest modification of anything inside, folders and links included. 0 if nothing is inside.
    pub newest_ms: i64,
    /// A folder directly inside.
    pub subfolders: bool,
    /// Anything that is not a folder (a link counts), at any depth.
    pub has_files: bool,
    /// A `.exe` or `.dll` anywhere inside.
    pub programs: bool,
    /// The first names directly inside, in alphabetical order.
    pub top: Vec<String>,
    /// Something online-only is inside: the folder is left out, never judged.
    pub cloud: bool,
}

/// What an entry is, from its attributes alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Dir,
    File,
    /// A junction or symlink: counted as something, never entered.
    Link,
    /// An online-only placeholder or a folder that fills in on access: never entered, never read.
    Cloud,
}

pub(crate) fn kind_of(attrs: u32) -> Kind {
    if is_cloud(attrs) {
        Kind::Cloud
    } else if attrs & ATTR_REPARSE != 0 {
        Kind::Link
    } else if attrs & ATTR_DIRECTORY != 0 {
        Kind::Dir
    } else {
        Kind::File
    }
}

fn is_program(name: &std::ffi::OsStr) -> bool {
    Path::new(name).extension().is_some_and(|e| e.eq_ignore_ascii_case("exe") || e.eq_ignore_ascii_case("dll"))
}

/// Keeps the `CONTENTS_SHOWN` alphabetically first names.
fn keep_name(top: &mut Vec<String>, name: String) {
    let key = |s: &str| s.to_lowercase();
    let at = top.partition_point(|t| key(t) <= key(&name));
    if at < CONTENTS_SHOWN {
        top.insert(at, name);
        top.truncate(CONTENTS_SHOWN);
    }
}

/// Walks `root` on this thread, reading metadata only. `None` if anything in it can't be read,
/// vanishes, or is a cloud placeholder: that folder is then left out of the list.
pub fn walk(root: &Path) -> Option<Summary> {
    let mut s = Summary::default();
    let mut stack = vec![(root.to_path_buf(), true)];
    while let Some((dir, top)) = stack.pop() {
        for entry in std::fs::read_dir(&dir).ok()? {
            let entry = entry.ok()?;
            let md = entry.metadata().ok()?;
            s.newest_ms = s.newest_ms.max(modified_ms(&md));
            if top {
                keep_name(&mut s.top, entry.file_name().to_string_lossy().into_owned());
            }
            match kind_of(md.file_attributes()) {
                Kind::Cloud => return None,
                Kind::Link => s.has_files = true,
                Kind::Dir => {
                    s.subfolders |= top;
                    stack.push((entry.path(), false));
                }
                Kind::File => {
                    s.has_files = true;
                    s.files += 1;
                    s.size = s.size.saturating_add(md.len());
                    s.programs |= is_program(&entry.file_name());
                }
            }
        }
    }
    Some(s)
}

/// The evidence a folder's name, contents and age give. Pure.
fn evidence(name: &str, s: &Summary, folder_ms: i64, now_ms: i64, installed: &InstalledNames) -> Vec<Evidence> {
    let mut ev = Vec::new();
    if !s.has_files {
        ev.push(Evidence::Empty);
    }
    if !s.subfolders {
        ev.push(Evidence::NoSubfolders);
    }
    let age = now_ms.saturating_sub(s.newest_ms.max(folder_ms));
    if age >= YEAR_MS {
        ev.push(Evidence::Stale);
    }
    if age <= MONTH_MS {
        ev.push(Evidence::RecentlyChanged);
    }
    if GENERIC_NAMES.iter().any(|g| g.eq_ignore_ascii_case(name)) {
        ev.push(Evidence::GenericName);
    }
    if s.programs {
        ev.push(Evidence::ContainsPrograms);
    }
    if s.files >= MANY_FILES {
        ev.push(Evidence::ManyFiles);
    }
    if installed.apps.iter().any(|a| names_are_similar(name, a)) {
        ev.push(Evidence::SimilarInstalledApp);
    }
    if installed.publishers.iter().any(|p| is_publisher_name(name, p)) {
        ev.push(Evidence::PublisherStillInstalled);
    }
    ev
}

/// A folder worth walking.
struct Candidate {
    path: PathBuf,
    parent: PathBuf,
    modified_ms: i64,
    in_program_files: bool,
}

/// The top-level folders of the scan roots that nothing claims. Links, cloud folders and
/// plain files are not candidates.
fn candidates(places: &Places, claims: &dyn ClaimCheck) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut seen_roots = HashSet::new();
    for root in places.scan_roots() {
        if !seen_roots.insert(canon(root)) {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(root) else { continue };
        for entry in rd.flatten() {
            let Ok(md) = entry.metadata() else { continue };
            if kind_of(md.file_attributes()) != Kind::Dir {
                continue;
            }
            let path = entry.path();
            let c = canon(&path);
            if never_listed(places, &c) || claims.is_claimed(&path).is_some() {
                continue;
            }
            out.push(Candidate { in_program_files: places.in_program_files(&c), modified_ms: modified_ms(&md), parent: root.clone(), path });
        }
    }
    out
}

/// What a scan found, and how many folders it could not judge.
#[derive(Debug, Default)]
pub(crate) struct Counted {
    pub rows: Vec<LeftoverRow>,
    /// Folders left out because they could not be read, vanished, or hold a cloud placeholder.
    /// Only the tests and the real-PC check read it; `scan` hands back the rows alone.
    #[cfg_attr(not(test), allow(dead_code))]
    pub left_out: usize,
}

fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The scan with its parts handed in, so tests can say what the walk finds and what time it is.
pub(crate) fn scan_with(
    places: &Places,
    claims: &dyn ClaimCheck,
    installed: &InstalledNames,
    now_ms: i64,
    index: &dyn IndexFacts,
    walker: &dyn Fn(&Path) -> Option<Summary>,
) -> Counted {
    let mut scored: Vec<(i32, LeftoverRow)> = Vec::new();
    let mut left_out = 0;
    let cands = candidates(places, claims);
    let paths: Vec<PathBuf> = cands.iter().map(|c| c.path.clone()).collect();
    let mut known = index.summaries(&paths);
    known.resize(cands.len(), None);
    for (c, from_index) in cands.into_iter().zip(known) {
        let found = match from_index {
            Some(s) => Some(s).filter(|s| !s.cloud),
            None => walker(&c.path),
        };
        let Some(s) = found else {
            left_out += 1;
            continue;
        };
        let name = c.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let (total, level, reasons) = score(&evidence(&name, &s, c.modified_ms, now_ms, installed));
        scored.push((
            total,
            LeftoverRow {
                path: c.path.to_string_lossy().into_owned(),
                parent: c.parent.to_string_lossy().into_owned(),
                size: s.size,
                modified: s.newest_ms.max(c.modified_ms),
                needs_admin: c.in_program_files,
                level,
                reasons,
                contents: s.top,
            },
        ));
    }
    // Most likely left over first, then the biggest.
    scored.sort_by(|(ta, a), (tb, b)| tb.cmp(ta).then(b.size.cmp(&a.size)).then_with(|| a.path.cmp(&b.path)));
    Counted { rows: scored.into_iter().map(|(_, r)| r).collect(), left_out }
}

/// Lists the leftover candidates of this PC. Folder facts come from `index` where it knows the
/// folder (Seek's own index, one question for all of them) and from one metadata walk where it
/// doesn't. Reads only; the caller decides what to do with the rows. `facts` is not asked
/// anything: it has no way to list a folder.
pub fn scan(
    places: &Places,
    claims: &dyn ClaimCheck,
    installed: &InstalledNames,
    _facts: &dyn FileFacts,
    index: &dyn IndexFacts,
) -> Vec<LeftoverRow> {
    scan_with(places, claims, installed, unix_now_ms(), index, &walk).rows
}

#[cfg(test)]
mod tests;
