//! The removal engine: `prepare` looks at the disk and hands back what the review sheet shows,
//! `commit` removes a subset of exactly that, re-checking everything at the last moment.
//! Safety rules 1, 3, 4, 5 and 7 live here (see the cleanup design spec). The only way out of
//! this module is `Recycler::recycle`, which can only fill the Recycle Bin.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::facts::{ClaimCheck, Elevator, FileFacts, Recycler, Stat};
use crate::model::{
    Candidate, CommitResult, ItemOutcome, JobItem, Level, Lock, Outcome, Prepared, Reason,
    ReviewItem, Why,
};
use crate::paths::{canon, real_location};
use crate::protect::{lock_for, Places};

/// Tokens that stay valid at once. Older ones are dropped.
const MAX_TOKENS: usize = 8;

const GONE: &str = "It is no longer there, so nothing was removed.";
const CHANGED: &str = "This changed after you reviewed it, so it was left in place.";
const LAST_COPY: &str = "This is the last copy, so it was left in place.";
const NO_TWIN: &str =
    "No other copy could be confirmed as identical and unchanged, so it was left in place.";
const NEEDS_ADMIN: &str = "Windows admin rights are needed for this.";
const STOPPED: &str = "Removal stopped after an item could not be confirmed in the Recycle Bin, so this was left in place.";
const DESTROYED: &str = "This is gone, but Windows did not confirm it in the Recycle Bin, so it may have been deleted for good.";

fn lock_reason(l: Lock) -> &'static str {
    match l {
        Lock::SystemFolder => "This is part of Windows, so it was left in place.",
        Lock::ProgramFiles => "This is in Program Files, so it was left in place.",
        Lock::OtherUser => "This belongs to another user, so it was left in place.",
        Lock::AppData => "This is in an app data folder, so it was left in place.",
        Lock::ToolFolder => "This is inside a tool folder (.git, node_modules and the like), so it was left in place.",
        Lock::NoRecycleBin => "This drive has no Recycle Bin that can hold it, so it was left in place.",
        Lock::CloudPlaceholder => "This is an online-only cloud file, so it was left in place.",
        Lock::Protected => "You marked this as always protected, so it was left in place.",
        Lock::LinkedPc => "This is on a linked PC. Seek only removes files on this PC, so it was left in place.",
        Lock::UnsupportedPath => "Seek cannot remove this kind of path, so it was left in place.",
        Lock::ReparsePoint => "This is a link to somewhere else, so it was left in place.",
        Lock::Claimed => "An app or a running program is using this folder, so it was left in place.",
    }
}

/// Everything `prepare` read about one item. `commit` compares the disk with this.
struct Rec {
    path: PathBuf,
    canon: String,
    why: Why,
    stat: Stat,
    file_id: Option<(u32, u64)>,
    group: Option<u32>,
    needs_admin: bool,
    lock: Option<Lock>,
}

/// One outstanding review. Items are keyed by id, which grows in the order they were listed, so
/// the first copy of a group is the one with the lowest id.
struct Review {
    recs: BTreeMap<u32, Rec>,
}

struct Seed {
    path: PathBuf,
    why: Why,
    group: Option<u32>,
    level: Option<Level>,
    reasons: Vec<Reason>,
}

pub struct Engine {
    places: Places,
    recycler: Arc<dyn Recycler>,
    elevator: Option<Arc<dyn Elevator>>,
    facts: Arc<dyn FileFacts>,
    claims: Arc<dyn ClaimCheck>,
    tokens: Mutex<VecDeque<(String, Review)>>,
}

impl Engine {
    pub fn new(
        places: Places,
        recycler: Arc<dyn Recycler>,
        elevator: Option<Arc<dyn Elevator>>,
        facts: Arc<dyn FileFacts>,
        claims: Arc<dyn ClaimCheck>,
    ) -> Engine {
        Engine {
            places,
            recycler,
            elevator,
            facts,
            claims,
            tokens: Mutex::new(VecDeque::new()),
        }
    }

    /// Rule 1: looks at each candidate on disk and returns what the review sheet shows, with a
    /// single-use token. Items that no longer exist are left out. Nothing here is selected.
    pub fn prepare(&self, candidates: Vec<Candidate>, user_protected: &[String]) -> Prepared {
        let mut built: Vec<(ReviewItem, Rec)> = Vec::new();
        let mut next_group = 0u32;
        for cand in candidates {
            let seed = |path, why, group, level, reasons| Seed {
                path,
                why,
                group,
                level,
                reasons,
            };
            match cand {
                Candidate::Duplicates { copies } => {
                    let g = next_group;
                    next_group += 1;
                    for p in copies {
                        self.add(
                            &mut built,
                            seed(p, Why::Duplicate, Some(g), None, vec![]),
                            user_protected,
                        );
                    }
                }
                Candidate::Folder {
                    path,
                    why,
                    level,
                    reasons,
                } => {
                    self.add(
                        &mut built,
                        seed(path, why, None, level, reasons),
                        user_protected,
                    );
                }
                Candidate::StaleFile { path } => {
                    self.add(
                        &mut built,
                        seed(path, Why::StaleData, None, None, vec![]),
                        user_protected,
                    );
                }
                Candidate::LinkedPc { path } => {
                    built.push(Self::linked_pc(built.len() as u32, path))
                }
            }
        }
        let token = new_token();
        let mut items = Vec::with_capacity(built.len());
        let mut recs = BTreeMap::new();
        for (item, rec) in built {
            recs.insert(item.id, rec);
            items.push(item);
        }
        let mut tokens = self.tokens.lock().unwrap();
        tokens.push_back((token.clone(), Review { recs }));
        while tokens.len() > MAX_TOKENS {
            tokens.pop_front();
        }
        Prepared { token, items }
    }

    fn linked_pc(id: u32, path: String) -> (ReviewItem, Rec) {
        let stat = Stat {
            size: 0,
            modified_ms: 0,
            is_dir: false,
            files: 1,
            reparse: false,
            cloud: false,
        };
        let item = ReviewItem {
            id,
            path: path.clone(),
            size: 0,
            modified_ms: 0,
            is_dir: false,
            files: 1,
            why: Why::Duplicate,
            lock: Some(Lock::LinkedPc),
            needs_admin: false,
            group: None,
            level: None,
            reasons: vec![],
        };
        let rec = Rec {
            canon: path.to_lowercase(),
            path: PathBuf::from(path),
            why: Why::Duplicate,
            stat,
            file_id: None,
            group: None,
            needs_admin: false,
            lock: Some(Lock::LinkedPc),
        };
        (item, rec)
    }

    fn add(&self, built: &mut Vec<(ReviewItem, Rec)>, seed: Seed, user_protected: &[String]) {
        let Some(stat) = self.facts.stat(&seed.path) else {
            return;
        };
        let id = built.len() as u32;
        let c = canon(&seed.path);
        let (lock, needs_admin) =
            self.judge(&seed.path, &c, seed.why, &stat, user_protected, stat.size);
        built.push((
            ReviewItem {
                id,
                path: seed.path.to_string_lossy().into_owned(),
                size: stat.size,
                modified_ms: stat.modified_ms,
                is_dir: stat.is_dir,
                files: stat.files,
                why: seed.why,
                lock,
                needs_admin,
                group: seed.group,
                level: seed.level,
                reasons: seed.reasons,
            },
            Rec {
                file_id: self.facts.file_id(&seed.path),
                path: seed.path,
                canon: c,
                why: seed.why,
                stat,
                group: seed.group,
                needs_admin,
                lock,
            },
        ));
    }

    /// Why this path can't be removed now (if it can't) and whether removing it needs admin.
    /// Used at `prepare` and again at `commit`. A path is judged under both spellings: as written
    /// and as it really is on disk (a junction into `C:\Windows` is Windows), the stricter wins.
    fn judge(
        &self,
        path: &Path,
        c: &str,
        why: Why,
        st: &Stat,
        user_protected: &[String],
        batch_bytes: u64,
    ) -> (Option<Lock>, bool) {
        let real = real_location(path);
        let mut lock = lock_for(&self.places, c, why, user_protected);
        if lock.is_none() {
            lock = match &real {
                Some(r) => lock_for(&self.places, r, why, user_protected),
                None => Some(Lock::UnsupportedPath), // exists but can't be located: fail closed
            };
        }
        let in_pf = |s: &str| self.places.in_program_files(s);
        let admin = why == Why::StaleData
            || (why == Why::Leftover && (in_pf(c) || real.as_deref().is_some_and(in_pf)));
        if lock.is_none() && st.cloud {
            lock = Some(Lock::CloudPlaceholder);
        }
        if lock.is_none() && st.reparse {
            lock = Some(Lock::ReparsePoint);
        }
        if lock.is_none() && st.is_dir && self.claims.is_claimed(path).is_some() {
            lock = Some(Lock::Claimed);
        }
        if lock.is_none() {
            lock = self.facts.recycle_block(path, batch_bytes);
        }
        (lock, admin)
    }

    /// Takes the token out of the store (single use) and returns its review.
    fn take(&self, token: &str) -> Option<Review> {
        let mut tokens = self.tokens.lock().unwrap();
        let pos = tokens.iter().position(|(t, _)| t == token)?;
        tokens.remove(pos).map(|(_, r)| r)
    }

    /// Rules 1, 3, 4, 5, 7: removes the chosen items of one review, or leaves each in place with
    /// a reason. The token is spent whatever happens. `user_protected` is read afresh.
    pub fn commit(
        &self,
        token: &str,
        ids: &[u32],
        user_protected: &[String],
    ) -> Result<CommitResult, String> {
        let review = self
            .take(token)
            .ok_or("This review is no longer valid. Review the items again.")?;
        let sel: BTreeSet<u32> = ids.iter().copied().collect();
        if sel.iter().any(|id| !review.recs.contains_key(id)) {
            return Err("Some of these items were not part of the review.".into());
        }

        let mut out: Vec<ItemOutcome> = Vec::new();
        let mut moved_bytes = 0u64;
        let mut stayed: HashSet<u32> = HashSet::new();
        let mut admin: Vec<JobItem> = Vec::new();
        let mut stopped: Option<String> = None;

        for &id in &sel {
            let rec = &review.recs[&id];
            let mk = |outcome| ItemOutcome {
                id,
                path: rec.path.to_string_lossy().into_owned(),
                size: rec.stat.size,
                outcome,
            };
            if stopped.is_some() {
                out.push(mk(Outcome::Left {
                    reason: STOPPED.into(),
                }));
                continue;
            }
            let verdict = self.check(&review, &sel, &stayed, id, rec, user_protected, moved_bytes);
            let outcome = match verdict {
                Err(reason) => Outcome::Left { reason },
                Ok(()) if rec.needs_admin => {
                    if self.elevator.is_some() {
                        admin.push(JobItem {
                            id,
                            path: rec.path.to_string_lossy().into_owned(),
                            size: rec.stat.size,
                            modified_ms: rec.stat.modified_ms,
                            why: rec.why,
                        });
                        continue;
                    }
                    Outcome::Left {
                        reason: NEEDS_ADMIN.into(),
                    }
                }
                // One path per call, right after its own checks: the gap between checking and
                // removing stays as small as it can be.
                Ok(()) => match self
                    .recycler
                    .recycle(std::slice::from_ref(&rec.path))
                    .into_iter()
                    .next()
                {
                    Some(Ok(bin_path)) => {
                        moved_bytes += rec.stat.size;
                        Outcome::Moved { bin_path }
                    }
                    failure => {
                        let why = failure
                            .and_then(Result::err)
                            .unwrap_or_else(|| "no answer".into());
                        if std::fs::symlink_metadata(&rec.path).is_ok() {
                            Outcome::Left { reason: format!("Windows could not move it to the Recycle Bin ({why}), so it was left in place.") }
                        } else {
                            // Gone, with no bin location: Windows deleted it for good. Stop everything.
                            stopped = Some(format!("{} could not be confirmed in the Recycle Bin ({why}). The rest was left untouched.", rec.path.display()));
                            out.insert(
                                0,
                                mk(Outcome::Left {
                                    reason: DESTROYED.into(),
                                }),
                            );
                            continue;
                        }
                    }
                },
            };
            if matches!(outcome, Outcome::Left { .. }) {
                stayed.insert(id);
            }
            out.push(mk(outcome));
        }

        if !admin.is_empty() {
            self.run_admin(&admin, &mut out, &mut moved_bytes, stopped.is_some());
        }
        Ok(CommitResult {
            items: out,
            moved_bytes,
            stopped_early: stopped,
        })
    }

    /// Sends the admin items through the elevator in one call. Every item gets an answer.
    fn run_admin(
        &self,
        jobs: &[JobItem],
        out: &mut Vec<ItemOutcome>,
        moved_bytes: &mut u64,
        stopped: bool,
    ) {
        let (answers, why) = match (&self.elevator, stopped) {
            (Some(e), false) => match e.run(jobs) {
                Ok(a) => (
                    a,
                    "The admin step did not remove this, so it was left in place.".to_string(),
                ),
                Err(e) => (
                    vec![],
                    format!("The admin step failed ({e}), so this was left in place."),
                ),
            },
            (_, true) => (vec![], STOPPED.to_string()),
            (None, false) => (vec![], NEEDS_ADMIN.to_string()),
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
                *moved_bytes += j.size;
            }
            out.push(ItemOutcome {
                id: j.id,
                path: j.path.clone(),
                size: j.size,
                outcome,
            });
        }
    }

    /// Rule 5 and the rules before it, for one item, reading the disk again.
    #[allow(clippy::too_many_arguments)]
    fn check(
        &self,
        review: &Review,
        sel: &BTreeSet<u32>,
        stayed: &HashSet<u32>,
        id: u32,
        rec: &Rec,
        user_protected: &[String],
        moved_bytes: u64,
    ) -> Result<(), String> {
        if let Some(l) = rec.lock {
            return Err(lock_reason(l).into());
        }
        let now = self.facts.stat(&rec.path).ok_or(GONE)?;
        let fid = self.facts.file_id(&rec.path);
        if now != rec.stat || fid.is_none() || fid != rec.file_id {
            return Err(CHANGED.into());
        }
        let (lock, _) = self.judge(
            &rec.path,
            &rec.canon,
            rec.why,
            &now,
            user_protected,
            moved_bytes + now.size,
        );
        if let Some(l) = lock {
            return Err(lock_reason(l).into());
        }
        match (rec.group, fid) {
            (Some(g), Some(fid)) => self.survivor(review, sel, stayed, id, rec, g, fid),
            _ => Ok(()),
        }
    }

    /// Rule 4: another copy of the group must stay, unchanged since review, a different file on
    /// disk (not a hard link) and byte for byte the same. Locked copies count.
    #[allow(clippy::too_many_arguments)]
    fn survivor(
        &self,
        review: &Review,
        sel: &BTreeSet<u32>,
        stayed: &HashSet<u32>,
        id: u32,
        rec: &Rec,
        group: u32,
        fid: (u32, u64),
    ) -> Result<(), String> {
        let mut candidates = 0;
        for (oid, o) in review
            .recs
            .iter()
            .filter(|(k, o)| **k != id && o.group == Some(group))
        {
            // Selected copies don't count, unless they were tried first and stayed.
            if sel.contains(oid) && !stayed.contains(oid) {
                continue;
            }
            candidates += 1;
            let unchanged = self.facts.stat(&o.path).as_ref() == Some(&o.stat)
                && o.file_id.is_some()
                && self.facts.file_id(&o.path) == o.file_id
                && o.file_id != Some(fid);
            if unchanged && self.facts.same_bytes(&rec.path, &o.path) == Ok(true) {
                return Ok(());
            }
        }
        Err(if candidates == 0 { LAST_COPY } else { NO_TWIN }.into())
    }
}

/// 128 random bits from the OS-seeded generator, as hex.
fn new_token() -> String {
    format!("{:032x}", rand::random::<u128>())
}

#[cfg(test)]
mod tests;
