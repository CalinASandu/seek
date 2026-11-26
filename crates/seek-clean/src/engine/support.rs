//! Small helpers of the engine that need no engine state.

use std::path::Path;

use crate::facts::Elevator;
use crate::model::{ItemOutcome, JobItem, Outcome};
use crate::paths::{canon, is_inside, real_location};
use crate::reasons::{ADMIN_SILENT, STOPPED};

/// Sends the admin items through the elevator in one call (not at all after a stop). Every item
/// gets an outcome; an item the elevator doesn't answer for might have been removed, and says so.
pub(super) fn run_admin(
    e: &dyn Elevator,
    jobs: &[JobItem],
    out: &mut Vec<ItemOutcome>,
    moved_bytes: &mut u64,
    stopped: &mut Option<String>,
) {
    let (answers, why) = if stopped.is_some() {
        (vec![], STOPPED.to_string())
    } else {
        match e.run(jobs) {
            Ok(a) => {
                *stopped = a.stopped_early;
                (a.items, ADMIN_SILENT.to_string())
            }
            Err(e) => (
                vec![],
                format!("The admin step failed ({e}). It may have removed some items first, so check the Recycle Bin."),
            ),
        }
    };
    for j in jobs {
        // Only an answer about an item we sent counts; anything else is ignored.
        let outcome = match answers.iter().find(|a| a.id == j.id) {
            Some(a) => a.outcome.clone(),
            None => Outcome::Left {
                reason: why.clone(),
            },
        };
        if matches!(outcome, Outcome::Moved { .. }) {
            *moved_bytes = moved_bytes.saturating_add(j.size);
        }
        out.push(ItemOutcome {
            id: j.id,
            path: j.path.clone(),
            size: j.size,
            outcome,
        });
    }
}

/// The path as written and as it really is, both in `canon` form.
pub(super) fn spellings(path: &Path) -> Vec<String> {
    let mut v = vec![canon(path)];
    v.extend(real_location(path).filter(|r| *r != v[0]));
    v
}

pub(super) fn inside_any(a: &[String], b: &[String]) -> bool {
    a.iter().any(|x| b.iter().any(|y| is_inside(x, y)))
}

/// Anything in `$Recycle.Bin` or `System Volume Information` is not a copy anyone keeps.
pub(super) fn in_system_store(c: &str) -> bool {
    c.split('\\')
        .any(|s| s == "$recycle.bin" || s == "system volume information")
}

/// 128 random bits from the OS-seeded generator, as hex.
pub(super) fn new_token() -> String {
    format!("{:032x}", rand::random::<u128>())
}
