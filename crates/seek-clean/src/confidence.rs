//! The confidence meter: each piece of evidence about a folder adds or takes away points with a
//! written reason, the total becomes a level, and the reasons stay visible. The idea is Bulk
//! Crap Uninstaller's (Apache-2.0); this is Seek's own version.
//!
//! The points are one table ([`POINTS`]) so they can be tuned once they have been seen on real
//! folders. Nothing here decides to remove anything: a high score is a suggestion.
//!
//! Name matching lives here too, because two of the rules are about names. Names of 4 letters or
//! fewer never count as a match (BCU's guard against `AppX Extended` being matched as junk from
//! `AppX`).

use crate::claims::words;
use crate::model::{Level, Reason};

/// One kind of evidence; one variant per row of the spec's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Evidence {
    /// Apps: inside the uninstalled app's recorded install folder.
    InInstallFolder,
    /// Apps: the folder's name is the uninstalled app's name exactly.
    ExactAppName,
    /// Apps: the parent folder is the uninstalled app's publisher.
    ParentIsPublisher,
    /// Apps: the name only roughly matches the app.
    RoughNameMatch,
    /// Apps: named after the company, not the product.
    NamedAfterCompany,
    /// Apps: one level of depth below the top (counted once per level, at most 2).
    Depth,
    /// Another installed app has a similar name.
    SimilarInstalledApp,
    /// Empty, or nothing left but empty folders.
    Empty,
    NoSubfolders,
    /// Nothing inside changed for a year.
    Stale,
    GenericName,
    ContainsPrograms,
    ManyFiles,
    /// The publisher still has an app installed.
    PublisherStillInstalled,
    /// Changed in the last 30 days.
    RecentlyChanged,
}

/// Evidence, points, and the sentence the window shows. Sentences say what was seen, not what to do.
pub const POINTS: &[(Evidence, i32, &str)] = &[
    (Evidence::InInstallFolder, 4, "Was inside an uninstalled app's install folder"),
    (Evidence::ExactAppName, 2, "Named exactly like an app that is no longer installed"),
    (Evidence::ParentIsPublisher, 4, "Its parent folder is named after an uninstalled app's publisher"),
    (Evidence::RoughNameMatch, -2, "Its name only roughly matches the app"),
    (Evidence::NamedAfterCompany, -2, "Named after the company, not the product"),
    (Evidence::Depth, -2, "Sits one level below the top"),
    (Evidence::SimilarInstalledApp, -2, "Another installed app has a similar name"),
    (Evidence::Empty, 4, "Empty, or nothing left but empty folders"),
    (Evidence::NoSubfolders, 1, "Has no subfolders"),
    (Evidence::Stale, 2, "Nothing inside changed for a year"),
    (Evidence::GenericName, -3, "Generic folder name"),
    (Evidence::ContainsPrograms, -4, "Contains programs (.exe, .dll)"),
    (Evidence::ManyFiles, -2, "Contains many files"),
    (Evidence::PublisherStillInstalled, -4, "The publisher still has an app installed"),
    (Evidence::RecentlyChanged, -3, "Changed in the last 30 days"),
];

/// Level boundaries, from the spec: 5 or more, 2 to 4, 0 to 1, below 0.
pub fn level(total: i32) -> Level {
    match total {
        5.. => Level::VeryLikely,
        2..=4 => Level::Likely,
        0..=1 => Level::NotSure,
        _ => Level::ProbablyInUse,
    }
}

fn row(ev: Evidence) -> (i32, &'static str) {
    POINTS
        .iter()
        .find(|(e, _, _)| *e == ev)
        .map(|(_, p, t)| (*p, *t))
        .expect("every Evidence variant has a row in POINTS")
}

/// Levels of depth that count (the spec: at most 2).
pub const MAX_DEPTH: usize = 2;

/// The total, its level, and one reason per piece of evidence (a repeated piece, such as
/// `Depth`, is repeated), the strongest first. Equal strength keeps the order given.
pub fn score(ev: &[Evidence]) -> (i32, Level, Vec<Reason>) {
    let mut depth = 0;
    let mut reasons: Vec<Reason> = ev
        .iter()
        // At most `MAX_DEPTH` levels of depth count, however many are given.
        .filter(|e| **e != Evidence::Depth || { depth += 1; depth <= MAX_DEPTH })
        .map(|e| {
            let (points, text) = row(*e);
            Reason { text: text.to_string(), points }
        })
        .collect();
    reasons.sort_by_key(|r| std::cmp::Reverse(r.points.abs()));
    let total = reasons.iter().map(|r| r.points).sum();
    (total, level(total), reasons)
}

// ---- names -------------------------------------------------------------------------------------

/// Letters and digits in the words: the length that the 4-letter rule counts.
fn name_len(w: &[String]) -> usize {
    w.iter().map(|s| s.chars().count()).sum()
}

/// True if `needle` appears in `hay` as a run of whole, consecutive words.
fn contains_run(hay: &[String], needle: &[String]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// True if either name's words are a whole-word run inside the other's: `Docker` and
/// `Docker Desktop`, `Epic Games` and `Epic Games Launcher`. Names of 4 letters or fewer
/// (counting the shorter of the two) never match, and neither does an empty name.
pub fn names_are_similar(folder: &str, app: &str) -> bool {
    let (f, a) = (words(folder), words(app));
    if name_len(&f) <= 4 || name_len(&a) <= 4 {
        return false;
    }
    contains_run(&a, &f) || contains_run(&f, &a)
}

/// Words that only say what kind of company a publisher is.
const COMPANY_WORDS: &[&str] = &[
    "inc", "llc", "ltd", "corp", "corporation", "gmbh", "co", "company", "software", "technologies",
];

fn without_company_words(w: Vec<String>) -> Vec<String> {
    w.into_iter().filter(|s| !COMPANY_WORDS.contains(&s.as_str())).collect()
}

/// True if the folder is named after this publisher: the same words once company suffixes are
/// dropped from both (`Google` and `Google LLC`; `NVIDIA` and `NVIDIA Corporation`), or the
/// folder is the leading words of a longer publisher name. 4 letters or fewer never match.
pub fn is_publisher_name(folder: &str, publisher: &str) -> bool {
    let folder = without_company_words(words(folder));
    if name_len(&folder) <= 4 {
        return false;
    }
    let raw = words(publisher);
    let trimmed = without_company_words(raw.clone());
    folder == trimmed || trimmed.starts_with(&folder) || raw.starts_with(&folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_has_every_spec_row_once() {
        assert_eq!(POINTS.len(), 15);
        for (i, (e, _, _)) in POINTS.iter().enumerate() {
            assert!(POINTS[i + 1..].iter().all(|(o, _, _)| o != e), "{e:?} twice");
        }
    }

    #[test]
    fn each_evidence_moves_the_score_by_its_table_value() {
        // The spec's table, written out again here so a change to POINTS has to change this too.
        let spec = [
            (Evidence::InInstallFolder, 4),
            (Evidence::ExactAppName, 2),
            (Evidence::ParentIsPublisher, 4),
            (Evidence::RoughNameMatch, -2),
            (Evidence::NamedAfterCompany, -2),
            (Evidence::Depth, -2),
            (Evidence::SimilarInstalledApp, -2),
            (Evidence::Empty, 4),
            (Evidence::NoSubfolders, 1),
            (Evidence::Stale, 2),
            (Evidence::GenericName, -3),
            (Evidence::ContainsPrograms, -4),
            (Evidence::ManyFiles, -2),
            (Evidence::PublisherStillInstalled, -4),
            (Evidence::RecentlyChanged, -3),
        ];
        assert_eq!(spec.len(), POINTS.len());
        for (ev, points) in spec {
            let (total, _, reasons) = score(&[ev]);
            assert_eq!(total, points, "{ev:?}");
            assert_eq!(reasons.len(), 1);
            assert_eq!(reasons[0].points, points, "{ev:?}");
        }
        // Evidence adds up, and a repeated piece counts each time.
        let (total, _, reasons) = score(&[Evidence::Empty, Evidence::Depth, Evidence::Depth, Evidence::Stale]);
        assert_eq!(total, 2);
        assert_eq!(reasons.len(), 4);
        assert_eq!(score(&[]).0, 0);
    }

    #[test]
    fn an_empty_folder_with_no_subfolders_alone_is_very_likely() {
        assert_eq!(score(&[Evidence::Empty, Evidence::NoSubfolders]).1, Level::VeryLikely);
        assert_eq!(score(&[Evidence::NoSubfolders]).1, Level::NotSure);
    }

    #[test]
    fn depth_counts_at_most_two_levels() {
        let (total, _, reasons) = score(&[Evidence::Depth, Evidence::Depth, Evidence::Depth]);
        assert_eq!(total, -4);
        assert_eq!(reasons.len(), 2);
    }

    #[test]
    fn level_boundaries() {
        assert_eq!(level(5), Level::VeryLikely);
        assert_eq!(level(40), Level::VeryLikely);
        assert_eq!(level(4), Level::Likely);
        assert_eq!(level(2), Level::Likely);
        assert_eq!(level(1), Level::NotSure);
        assert_eq!(level(0), Level::NotSure);
        assert_eq!(level(-1), Level::ProbablyInUse);
        assert_eq!(level(-30), Level::ProbablyInUse);
    }

    #[test]
    fn score_names_its_level_and_lists_the_strongest_reason_first() {
        let (total, lvl, reasons) = score(&[Evidence::Stale, Evidence::Empty, Evidence::GenericName]);
        assert_eq!((total, lvl), (3, Level::Likely));
        let order: Vec<i32> = reasons.iter().map(|r| r.points).collect();
        assert_eq!(order, vec![4, -3, 2]);
    }

    #[test]
    fn names_of_four_letters_or_fewer_never_match() {
        assert!(!names_are_similar("Adob", "Adobe Acrobat"));
        assert!(!names_are_similar("Adobe Acrobat", "Adob"));
        assert!(!names_are_similar("AppX", "AppX Extended"));
        assert!(!names_are_similar("A B C", "A B C Studio"));
        assert!(!is_publisher_name("Nvid", "Nvid Corporation"));
        assert!(!is_publisher_name("Co", "Co Ltd"));
        assert!(!is_publisher_name("Inc Llc", "Inc"));
        // Five letters is enough.
        assert!(names_are_similar("Adobe", "Adobe Acrobat"));
        assert!(is_publisher_name("Adobe", "Adobe Inc."));
    }

    #[test]
    fn a_similar_named_app_is_penalized() {
        assert!(names_are_similar("Docker", "Docker Desktop"));
        assert!(names_are_similar("Epic Games", "Epic Games Launcher"));
        assert!(names_are_similar("Ollama", "Ollama"));
        // The reverse: the app's name sits inside the folder's.
        assert!(names_are_similar("Docker Desktop Cache", "Docker Desktop"));
        // Whole words only, and in order.
        assert!(!names_are_similar("Dock", "Docker Desktop"));
        assert!(!names_are_similar("Games Epic", "Epic Games Launcher"));
        assert!(!names_are_similar("Doc", "Docker Desktop"));
        assert!(!names_are_similar("Foobar", "Foo"));
        assert!(!names_are_similar("Something Else", "Docker Desktop"));
        let (total, _, reasons) = score(&[Evidence::SimilarInstalledApp]);
        assert_eq!(total, -2);
        assert!(reasons[0].text.contains("similar name"));
    }

    #[test]
    fn publisher_names_match_without_their_company_words() {
        assert!(is_publisher_name("Google", "Google LLC"));
        assert!(is_publisher_name("NVIDIA", "NVIDIA Corporation"));
        assert!(is_publisher_name("NVIDIA Corporation", "NVIDIA Corporation"));
        assert!(is_publisher_name("nvidia corporation", "NVIDIA, Inc."));
        assert!(is_publisher_name("Adobe", "Adobe Systems Incorporated"));
        assert!(is_publisher_name("Mozilla", "Mozilla Foundation"));
        assert!(!is_publisher_name("Google", "Alphabet Inc"));
        assert!(!is_publisher_name("Software", "Software Inc"));
        assert!(!is_publisher_name("Googles", "Google LLC"));
        assert!(!is_publisher_name("Mozilla Firefox", "Mozilla Foundation"));
    }

    #[test]
    fn every_reason_is_a_plain_sentence() {
        for (ev, _, text) in POINTS {
            assert!(text.len() > 10, "{ev:?}");
            assert!(text.chars().next().unwrap().is_uppercase(), "{text}");
            assert!(!text.ends_with(['.', ':', ';']), "{text}");
            assert!(!text.contains(['{', '}', '_', '%']), "{text}");
            assert!(!text.to_lowercase().contains("safe"), "{text}");
        }
        let texts: std::collections::HashSet<_> = POINTS.iter().map(|(_, _, t)| *t).collect();
        assert_eq!(texts.len(), POINTS.len(), "two rows share a sentence");
    }
}
