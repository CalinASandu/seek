//! What one uninstalled app left behind: its install folder (if the app itself recorded it),
//! folders in Program Files and AppData that carry its name or sit under its publisher's folder,
//! and Start Menu shortcuts. Rows are scored with the spec's table (the Apps rows) and are never
//! ticked.
//!
//! The rules, all from the spec:
//! - A folder found by name or publisher is at most two levels below a scan root.
//! - Names of 4 letters or fewer never match.
//! - The install folder is a row only when its source is `InstallLocation`. A guessed folder is
//!   never a row; what is inside it counts only through the name and publisher matches.
//! - Folder facts come from the index where it knows the folder and from [`walk`] where it
//!   doesn't, the same as the Leftovers scan. Nothing is opened.

use std::collections::HashSet;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::{evidence, kind_of, unix_now_ms, walk, IndexFacts, Kind, Summary};
use crate::claims::{expand_env, is_fixed_local, link_targets, whole_words_match, words, InstalledNames};
use crate::confidence::{is_publisher_name, name_len, names_are_similar, same_publisher, score, Evidence, MAX_DEPTH};
use crate::facts::{modified_ms, ClaimCheck};
use crate::model::{AppRow, FolderSource, LeftoverRow};
use crate::paths::{canon, is_inside};
use crate::protect::{never_listed, uninstall_blocked, Places};

/// How a folder's or a shortcut's name compares with the app's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NameMatch {
    /// The same words (a trailing build word such as `(x64)` ignored).
    Exact,
    /// One name holds the other as whole, consecutive words: `Acme Editor Plus` for `Acme Editor`.
    Rough,
    None,
}

/// Names of 4 letters or fewer never match, on either side.
pub(crate) fn match_name(folder: &str, app: &str) -> NameMatch {
    if name_len(&words(folder)) <= 4 || name_len(&words(app)) <= 4 {
        NameMatch::None
    } else if words(folder) == words(app) {
        NameMatch::Exact
    } else if whole_words_match(folder, app) || names_are_similar(folder, app) {
        // A match that needed build words trimmed (`Python 3.12.4 (64-bit)` for `Python`) is rough.
        NameMatch::Rough
    } else {
        NameMatch::None
    }
}

fn is_publisher(folder: &str, publisher: &str) -> bool {
    !publisher.trim().is_empty() && is_publisher_name(folder, publisher)
}

/// A folder worth listing, with what its place and name say about it.
struct Hit {
    path: PathBuf,
    canon: String,
    modified_ms: i64,
    in_program_files: bool,
    /// Evidence from where it is and what it is called; its contents are judged from the summary.
    found: Vec<Evidence>,
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn is_real_dir(path: &Path) -> Option<std::fs::Metadata> {
    let md = std::fs::symlink_metadata(path).ok()?;
    (kind_of(md.file_attributes()) == Kind::Dir).then_some(md)
}

/// Levels below the scan root that holds `path` (a direct child of a root is level 0), or 0 when
/// no scan root holds it.
fn level_below_root(places: &Places, c: &str) -> usize {
    places
        .scan_roots()
        .iter()
        .map(|r| canon(r))
        .filter(|r| c.len() > r.len() && is_inside(c, r))
        .map(|r| c[r.len()..].trim_matches('\\').split('\\').count().saturating_sub(1))
        .max()
        .unwrap_or(0)
}

fn depth_evidence(level: usize) -> impl Iterator<Item = Evidence> {
    std::iter::repeat_n(Evidence::Depth, level.min(MAX_DEPTH))
}

/// The folders at most two levels below the scan roots that carry the app's name or sit directly
/// under a folder named after its publisher. A folder that fails `never_listed` is neither listed
/// nor entered; one that something else claims is not listed but is entered.
fn name_hits(app: &AppRow, places: &Places, claims: &dyn ClaimCheck, recorded: Option<&str>) -> Vec<Hit> {
    let mut out = Vec::new();
    let mut seen_roots = HashSet::new();
    for root in places.scan_roots() {
        if !seen_roots.insert(canon(root)) {
            continue;
        }
        // (folder to read, the level of what is in it, the name of the folder to read when it is below the root)
        let mut stack: Vec<(PathBuf, usize, Option<String>)> = vec![(root.clone(), 0, None)];
        while let Some((dir, level, parent_name)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for entry in rd.flatten() {
                let Ok(md) = entry.metadata() else { continue };
                if kind_of(md.file_attributes()) != Kind::Dir {
                    continue;
                }
                let path = entry.path();
                let c = canon(&path);
                // Neither listed nor entered: what Windows and other apps share, and what the
                // engine would lock anyway.
                if never_listed(places, &c) || uninstall_blocked(places, &c) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let matched = match_name(&name, &app.name);
                let under_publisher = parent_name.as_deref().is_some_and(|p| is_publisher(p, &app.publisher));
                // Every row needs its own name match; sitting under the publisher's folder is only evidence.
                // The publisher's own folder is searched into and is never a row.
                if matched != NameMatch::None && !is_publisher(&name, &app.publisher) && claims.is_claimed(&path).is_none() {
                    let mut found = Vec::new();
                    match matched {
                        NameMatch::Exact => found.push(Evidence::ExactAppName),
                        NameMatch::Rough => found.push(Evidence::RoughNameMatch),
                        NameMatch::None => {}
                    }
                    if under_publisher {
                        found.push(Evidence::ParentIsPublisher);
                    }
                    if recorded.is_some_and(|r| is_inside(&c, r)) {
                        found.push(Evidence::InInstallFolder);
                    }
                    found.extend(depth_evidence(level));
                    out.push(Hit { in_program_files: places.in_program_files(&c), modified_ms: modified_ms(&md), canon: c, path: path.clone(), found });
                }
                if level < MAX_DEPTH {
                    stack.push((path, level + 1, Some(name)));
                }
            }
        }
    }
    out
}

/// The install folder as a row, when the app recorded it itself and it is still there.
fn install_hit(app: &AppRow, places: &Places) -> Option<Hit> {
    if app.folder_source != Some(FolderSource::InstallLocation) {
        return None;
    }
    let path = PathBuf::from(app.folder.as_deref()?);
    let c = canon(&path);
    if never_listed(places, &c) || places.is_or_contains_place(&c) || uninstall_blocked(places, &c) {
        return None;
    }
    let md = is_real_dir(&path)?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    let mut found = vec![Evidence::InInstallFolder];
    if match_name(&name, &app.name) == NameMatch::Exact {
        found.push(Evidence::ExactAppName);
    }
    found.extend(depth_evidence(level_below_root(places, &c)));
    Some(Hit { in_program_files: places.in_program_files(&c), modified_ms: modified_ms(&md), canon: c, path, found })
}

/// Every row loses the points when another listed app has this app's publisher (compared with
/// company suffixes dropped). Not added twice.
fn add_publisher_still_installed(ev: &mut Vec<Evidence>, app: &AppRow, installed: &InstalledNames) {
    let shared = !app.publisher.trim().is_empty() && installed.publishers.iter().any(|p| same_publisher(&app.publisher, p));
    if shared && !ev.contains(&Evidence::PublisherStillInstalled) {
        ev.push(Evidence::PublisherStillInstalled);
    }
}

/// The `.lnk` files in the Start Menu `Programs` folders, links not followed.
fn shortcut_files(places: &Places) -> Vec<PathBuf> {
    const MAX_FILES: usize = 20_000;
    const MAX_DEPTH_DOWN: usize = 8;
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for root in places.start_menu_roots() {
        if !seen.insert(canon(root)) {
            continue;
        }
        let mut stack = vec![(root.clone(), 0)];
        while let Some((dir, depth)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for entry in rd.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                let path = entry.path();
                if ft.is_dir() && !ft.is_symlink() && depth < MAX_DEPTH_DOWN {
                    stack.push((path, depth + 1));
                } else if ft.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) && out.len() < MAX_FILES {
                    out.push(path);
                }
            }
        }
    }
    out
}

/// Start Menu shortcuts of the app, and only those that no longer work: the target is gone (or the
/// shortcut can't be read, and it is named exactly like the app). A shortcut that still reaches
/// something is another program's or another install's, and is never listed. Of the dead ones,
/// those pointing into the folder the app itself recorded, and those named exactly like the app.
fn shortcut_rows(
    app: &AppRow,
    places: &Places,
    recorded: Option<&str>,
    installed: &InstalledNames,
    links: &dyn Fn(&[PathBuf]) -> Vec<Option<String>>,
) -> Vec<(i32, LeftoverRow)> {
    let folder = recorded;
    let files = shortcut_files(places);
    let mut targets = links(&files);
    targets.resize(files.len(), None);
    let mut out = Vec::new();
    for (file, target) in files.into_iter().zip(targets) {
        let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let target = target.map(|t| expand_env(&t, &env_var)).map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        let target_canon = target.as_deref().map(|t| canon(Path::new(t)));
        let points_in = matches!((&target_canon, folder), (Some(t), Some(f)) if is_inside(t, f));
        let exact = match_name(&stem, &app.name) == NameMatch::Exact;
        // A target on a drive that may stall isn't looked at: such a shortcut is taken as working.
        let works = target.as_deref().is_some_and(|t| !is_fixed_local(t) || Path::new(t).exists());
        if works || !(points_in || exact) {
            continue;
        }
        let c = canon(&file);
        if !places.is_start_menu_shortcut(&c) {
            continue;
        }
        let Ok(md) = std::fs::symlink_metadata(&file) else { continue };
        let mut ev = Vec::new();
        if points_in {
            ev.push(Evidence::InInstallFolder);
        }
        if exact {
            ev.push(Evidence::ExactAppName);
        }
        add_publisher_still_installed(&mut ev, app, installed);
        let (total, level, reasons) = score(&ev);
        out.push((
            total,
            LeftoverRow {
                path: file.to_string_lossy().into_owned(),
                parent: file.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
                size: md.len(),
                modified: modified_ms(&md),
                needs_admin: places.in_program_data(&c),
                level,
                reasons,
                contents: Vec::new(),
            },
        ));
    }
    out
}

/// The leftovers of one uninstalled app, with the parts handed in so tests can say what time it is,
/// what the index knows and what the shortcuts point at. `links` reads the targets of a batch of
/// `.lnk` files (`None` for one it can't read).
#[allow(clippy::too_many_arguments)]
pub(crate) fn for_app_with(
    app: &AppRow,
    places: &Places,
    claims: &dyn ClaimCheck,
    installed: &InstalledNames,
    now_ms: i64,
    index: &dyn IndexFacts,
    walker: &dyn Fn(&Path) -> Option<Summary>,
    links: &dyn Fn(&[PathBuf]) -> Vec<Option<String>>,
) -> Vec<LeftoverRow> {
    let install = install_hit(app, places);
    // "Inside the recorded install folder" only means something when the app recorded it itself.
    let recorded = app.folder.as_deref().filter(|_| app.folder_source == Some(FolderSource::InstallLocation)).map(|f| canon(Path::new(f)));
    let mut hits: Vec<Hit> = install.into_iter().collect();
    let install_canon = hits.first().map(|h| h.canon.clone());
    let mut found = name_hits(app, places, claims, recorded.as_deref());
    // The install folder covers what is inside it, and is not swallowed by a folder above it.
    found.retain(|h| {
        install_canon.as_deref().is_none_or(|i| !is_inside(&h.canon, i) && !is_inside(i, &h.canon))
    });
    let mut seen = HashSet::new();
    found.retain(|h| seen.insert(h.canon.clone()));
    let outer: Vec<String> = found.iter().map(|h| h.canon.clone()).collect();
    found.retain(|h| !outer.iter().any(|o| *o != h.canon && is_inside(&h.canon, o)));
    hits.extend(found);

    let paths: Vec<PathBuf> = hits.iter().map(|h| h.path.clone()).collect();
    let mut known = index.summaries(&paths);
    known.resize(hits.len(), None);
    let mut scored: Vec<(i32, LeftoverRow)> = Vec::new();
    for (hit, from_index) in hits.into_iter().zip(known) {
        let summary = match from_index {
            Some(s) => Some(s).filter(|s| !s.cloud),
            None => walker(&hit.path),
        };
        let Some(s) = summary else { continue };
        let name = hit.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut ev = hit.found.clone();
        ev.extend(evidence(&name, &s, hit.modified_ms, now_ms, installed));
        add_publisher_still_installed(&mut ev, app, installed);
        let (total, level, reasons) = score(&ev);
        scored.push((
            total,
            LeftoverRow {
                path: hit.path.to_string_lossy().into_owned(),
                parent: hit.path.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
                size: s.size,
                modified: s.newest_ms.max(hit.modified_ms),
                needs_admin: hit.in_program_files,
                level,
                reasons,
                contents: s.top,
            },
        ));
    }
    scored.extend(shortcut_rows(app, places, recorded.as_deref(), installed, links));
    scored.sort_by(|(ta, a), (tb, b)| tb.cmp(ta).then(b.size.cmp(&a.size)).then_with(|| a.path.cmp(&b.path)));
    scored.into_iter().map(|(_, r)| r).collect()
}

/// The leftovers of the uninstalled `app` on this PC: nothing is removed, nothing is selected.
pub fn for_app(
    app: &AppRow,
    places: &Places,
    claims: &dyn ClaimCheck,
    installed: &InstalledNames,
    index: &dyn IndexFacts,
) -> Vec<LeftoverRow> {
    for_app_with(app, places, claims, installed, unix_now_ms(), index, &walk, &link_targets)
}
