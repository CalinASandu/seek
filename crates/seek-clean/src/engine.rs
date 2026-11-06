//! The removal engine: `prepare` looks at the disk and hands back what the review sheet shows,
//! `commit` removes a subset of exactly that, re-checking everything at the last moment.
//! Safety rules 1, 3, 4, 5 and 7 live here (see the cleanup design spec). The only way out of
//! this module is `Recycler::recycle`, which can only fill the Recycle Bin.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::facts::{ClaimCheck, Elevator, FileFacts, Recycler, Stat};
use crate::model::{
    Candidate, CommitResult, ItemOutcome, JobItem, Level, Lock, Outcome, Prepared, Reason,
    ReviewItem, Why,
};
use crate::paths::{canon, is_inside, real_location, target_location};
use crate::protect::{candidate_lock, in_shared_folder, lock_for, uninstall_lock, Places};
use crate::reasons::*;
use support::{in_system_store, inside_any, new_token, run_admin, spellings};

/// Tokens that stay valid at once. Older ones are dropped.
const MAX_TOKENS: usize = 8;

/// Everything `prepare` read about one item. `commit` compares the disk with this.
struct Rec {
    path: PathBuf,
    canon: String,
    why: Why,
    stat: Stat,
    file_id: Option<(u32, u64)>,
    group: Option<u32>,
    lock: Option<Lock>,
}

/// One outstanding review. Items are keyed by id, which grows in the order they were listed, so
/// the first copy of a group is the one with the lowest id.
struct Review {
    recs: BTreeMap<u32, Rec>,
}

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    File,
    Dir,
}

struct Seed {
    path: PathBuf,
    why: Why,
    shape: Shape,
    group: Option<u32>,
    level: Option<Level>,
    reasons: Vec<Reason>,
}

/// Per-call state for judging: the user's protected entries (as written and as they really are,
/// in `canon` form) and the bytes queued for the admin step on each drive: not in the bin yet, so
/// the bin's own size does not include them. Moved items are not added; the bin counts them.
struct Ctx {
    prot: Vec<String>,
    planned: HashMap<String, u64>,
}

impl Ctx {
    fn new(user_protected: &[String]) -> Ctx {
        let mut prot = Vec::new();
        for p in user_protected {
            prot.push(canon(Path::new(p)));
            // A protected link protects what it points at, and its own parents' real spelling.
            prot.extend(real_location(Path::new(p)));
            prot.extend(target_location(Path::new(p)));
        }
        Ctx {
            prot,
            planned: HashMap::new(),
        }
    }

    fn plan(&mut self, drive: &str, size: u64) {
        let t = self.planned.entry(drive.to_string()).or_insert(0);
        *t = t.saturating_add(size);
    }
}

/// What a commit works from.
struct Plan<'a> {
    review: &'a Review,
    sel: &'a BTreeSet<u32>,
    spell: &'a HashMap<u32, Vec<String>>,
}

struct Checked<'a> {
    admin: bool,
    twin: Option<&'a Rec>,
    drive: String,
}

enum Step {
    Moved(String),
    Stayed(String),
    Stop(String),
}

/// Held across the last look at an item and its move to the Recycle Bin. The normal one holds
/// nothing; the elevated process holds the folders above the item so none can be swapped for a
/// link in between. `Err` is the reason the item stays.
pub trait Guard: Send + Sync {
    fn hold(&self, path: &Path) -> Result<Box<dyn std::any::Any + Send>, String>;
}

/// Holds nothing.
pub struct NoGuard;

impl Guard for NoGuard {
    fn hold(&self, _path: &Path) -> Result<Box<dyn std::any::Any + Send>, String> {
        Ok(Box::new(()))
    }
}

pub struct Engine {
    guard: Arc<dyn Guard>,
    places: Places,
    recycler: Arc<dyn Recycler>,
    elevator: Option<Arc<dyn Elevator>>,
    /// True only inside the elevated process: items that need admin are recycled there and then,
    /// not handed to an elevator or locked as needing admin.
    elevated: bool,
    facts: Arc<dyn FileFacts>,
    claims: Arc<dyn ClaimCheck>,
    tokens: Mutex<VecDeque<(String, Review)>>,
    /// Held for the whole of every `commit`: two commits never judge each other's survivors.
    serial: Mutex<()>,
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
            guard: Arc::new(NoGuard),
            places,
            recycler,
            elevator,
            elevated: false,
            facts,
            claims,
            tokens: Mutex::new(VecDeque::new()),
            serial: Mutex::new(()),
        }
    }

    /// Replaces the guard held around each item's last look and recycle.
    pub fn with_guard(mut self, guard: Arc<dyn Guard>) -> Engine {
        self.guard = guard;
        self
    }

    /// The engine of the elevated process (`elevated::run_main`): the same rules and the same
    /// recycling, but items that need admin are removed here, since this process has it. It has
    /// no elevator, so it can never ask for another prompt.
    pub fn new_elevated(
        places: Places,
        recycler: Arc<dyn Recycler>,
        facts: Arc<dyn FileFacts>,
        claims: Arc<dyn ClaimCheck>,
    ) -> Engine {
        let mut e = Engine::new(places, recycler, None, facts, claims);
        e.elevated = true;
        e
    }

    /// Rule 1: looks at each candidate on disk and returns what the review sheet shows, with a
    /// single-use token. Items that no longer exist are left out. Nothing here is selected.
    pub fn prepare(&self, candidates: Vec<Candidate>, user_protected: &[String]) -> Prepared {
        let ctx = Ctx::new(user_protected);
        let mut built: Vec<(ReviewItem, Rec)> = Vec::new();
        let mut next_group = 0u32;
        let seed = |path, why, shape, group, level, reasons| Seed {
            path,
            why,
            shape,
            group,
            level,
            reasons,
        };
        for cand in candidates {
            match cand {
                Candidate::Duplicates { copies } => {
                    let g = next_group;
                    next_group += 1;
                    for p in copies {
                        let s = seed(p, Why::Duplicate, Shape::File, Some(g), None, vec![]);
                        self.add(&mut built, s, &ctx);
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
                        seed(path, why, Shape::Dir, None, level, reasons),
                        &ctx,
                    );
                }
                Candidate::Shortcut {
                    path,
                    level,
                    reasons,
                } => {
                    let s = seed(path, Why::Uninstall, Shape::File, None, level, reasons);
                    self.add(&mut built, s, &ctx);
                }
                Candidate::StaleFile { path } => {
                    let s = seed(path, Why::StaleData, Shape::File, None, None, vec![]);
                    self.add(&mut built, s, &ctx);
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
            lock: Some(Lock::LinkedPc),
        };
        (item, rec)
    }

    fn add(&self, built: &mut Vec<(ReviewItem, Rec)>, seed: Seed, ctx: &Ctx) {
        let Some(stat) = self.facts.stat(&seed.path) else {
            return;
        };
        let id = built.len() as u32;
        let c = canon(&seed.path);
        let (mut lock, needs_admin, _) = self.judge(&seed.path, &c, seed.why, &stat, ctx);
        // A folder candidate is a folder with a folder's purpose; a file candidate is a file.
        let right_kind = stat.is_dir == (seed.shape == Shape::Dir)
            && (seed.shape == Shape::File || matches!(seed.why, Why::Leftover | Why::Uninstall));
        if !right_kind {
            lock = lock.or(Some(Lock::UnsupportedPath));
        }
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
                lock,
            },
        ));
    }

    /// Why this path can't be removed now (if it can't), whether removing it needs admin, and its
    /// drive. Used at `prepare` and again at `commit`. A path is judged under both spellings: as
    /// written and as it really is on disk (a junction into `C:\Windows` is Windows). A protected
    /// entry counts when it holds the path or sits inside it.
    fn judge(
        &self,
        path: &Path,
        c: &str,
        why: Why,
        st: &Stat,
        ctx: &Ctx,
    ) -> (Option<Lock>, bool, String) {
        let real = real_location(path);
        let spell: Vec<&str> = std::iter::once(c).chain(real.as_deref()).collect();
        let mut lock = spell
            .iter()
            .find_map(|s| lock_for(&self.places, s, why, &ctx.prot));
        if lock.is_none() && real.is_none() {
            lock = Some(Lock::UnsupportedPath); // exists but can't be located: fail closed
        }
        if lock.is_none() && st.is_dir && spell.iter().any(|s| self.places.is_or_contains_place(s))
        {
            lock = Some(Lock::SystemFolder);
        }
        if lock.is_none()
            && ctx
                .prot
                .iter()
                .any(|p| spell.iter().any(|s| is_inside(p, s)))
        {
            lock = Some(Lock::Protected);
        }
        if lock.is_none() {
            lock = candidate_lock(&self.places, c, why);
        }
        // What an uninstall left: a folder that is not the user's own, or a Start Menu shortcut.
        // Under both spellings, so a junction can't carry a shortcut out of the Start Menu.
        if lock.is_none() && why == Why::Uninstall {
            lock = spell.iter().find_map(|s| uninstall_lock(&self.places, s, st.is_dir));
        }
        // Folders shared with Windows and other apps (Roaming\Microsoft\Protect, Microsoft\Edge and the
        // like): nothing inside them is ever an uninstall's to offer, however it is asked for.
        if lock.is_none() && why == Why::Uninstall && st.is_dir && spell.iter().any(|s| in_shared_folder(&self.places, s)) {
            lock = Some(Lock::SharedFolder);
        }
        let pf = |s: &&str| self.places.in_program_files(s);
        // Program Files, and the all-users Start Menu in ProgramData, can't be changed without admin.
        let admin = why == Why::StaleData
            || (matches!(why, Why::Leftover | Why::Uninstall) && spell.iter().any(pf))
            || (why == Why::Uninstall && spell.iter().any(|s| self.places.in_program_data(s)));
        if lock.is_none() && st.cloud {
            lock = Some(Lock::CloudPlaceholder);
        }
        if lock.is_none() && st.reparse {
            lock = Some(Lock::ReparsePoint);
        }
        let claimed = |p: &Path| self.claims.is_claimed(p).is_some();
        if lock.is_none()
            && st.is_dir
            && (claimed(path) || real.as_deref().is_some_and(|r| claimed(Path::new(r))))
        {
            lock = Some(Lock::Claimed);
        }
        let drive: String = real.as_deref().unwrap_or(c).chars().take(2).collect();
        if lock.is_none() {
            let batch = ctx
                .planned
                .get(&drive)
                .copied()
                .unwrap_or(0)
                .saturating_add(st.size);
            lock = self.facts.recycle_block(path, batch);
        }
        (lock, admin, drive)
    }

    /// Forgets every outstanding review: their tokens stop working (the sheet reports it expired)
    /// and what they remembered about the disk is freed. Removes nothing.
    pub fn drop_reviews(&self) {
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
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
        let _serial = self.serial.lock().unwrap_or_else(|e| e.into_inner());
        let review = self
            .take(token)
            .ok_or("This review is no longer valid. Review the items again.")?;
        let sel: BTreeSet<u32> = ids.iter().copied().collect();
        if sel.iter().any(|id| !review.recs.contains_key(id)) {
            return Err("Some of these items were not part of the review.".into());
        }
        let spell: HashMap<u32, Vec<String>> = sel
            .iter()
            .map(|id| (*id, spellings(&review.recs[id].path)))
            .collect();
        let dirs: Vec<u32> = sel
            .iter()
            .copied()
            .filter(|i| review.recs[i].stat.is_dir)
            .collect();
        let inner: HashSet<u32> = sel
            .iter()
            .copied()
            .filter(|x| {
                dirs.iter()
                    .any(|d| d != x && inside_any(&spell[x], &spell[d]))
            })
            .collect();
        let plan = Plan {
            review: &review,
            sel: &sel,
            spell: &spell,
        };

        let mut ctx = Ctx::new(user_protected);
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
            let left = |reason: &str| Outcome::Left {
                reason: reason.into(),
            };
            let outcome = if stopped.is_some() {
                left(STOPPED)
            } else if inner.contains(&id) {
                left(NESTED)
            } else {
                // Held from the last look until the item is moved (the elevated process holds the
                // folders above it, so none can be swapped for a link in between).
                let _hold = match self.guard.hold(&rec.path) {
                    Ok(h) => h,
                    Err(why) => {
                        stayed.insert(id);
                        out.push(mk(left(&why)));
                        continue;
                    }
                };
                match self.check(&plan, &stayed, id, &ctx) {
                    Err(reason) => left(&reason),
                    // The last look before removal, so the byte comparison above can't have gone stale.
                    Ok(c) if !self.unchanged(rec) || c.twin.is_some_and(|t| !self.unchanged(t)) => left(CHANGED),
                    Ok(c) if c.admin && self.elevator.is_some() => {
                        ctx.plan(&c.drive, rec.stat.size);
                        admin.push(JobItem {
                            id,
                            path: rec.path.to_string_lossy().into_owned(),
                            size: rec.stat.size,
                            modified_ms: rec.stat.modified_ms,
                            why: rec.why,
                        });
                        continue;
                    }
                    Ok(c) if c.admin && !self.elevated => left(NEEDS_ADMIN),
                    // One path per call, straight after its own last look.
                    Ok(_) => match self.recycle_step(rec) {
                        Step::Moved(bin_path) => {
                            moved_bytes = moved_bytes.saturating_add(rec.stat.size);
                            Outcome::Moved { bin_path }
                        }
                        Step::Stayed(why) => {
                            left(&format!("Windows could not move it to the Recycle Bin ({why}), so it was left in place."))
                        }
                        Step::Stop(why) => {
                            stopped = Some(format!("{} could not be confirmed in the Recycle Bin ({why}). The rest was left untouched.", rec.path.display()));
                            out.insert(0, mk(left(DESTROYED)));
                            continue;
                        }
                    },
                }
            };
            if matches!(outcome, Outcome::Left { .. }) {
                stayed.insert(id);
            }
            out.push(mk(outcome));
        }

        if let Some(e) = self.elevator.as_ref().filter(|_| !admin.is_empty()) {
            run_admin(
                e.as_ref(),
                &admin,
                &mut out,
                &mut moved_bytes,
                &mut stopped,
            );
        }
        Ok(CommitResult {
            items: out,
            moved_bytes,
            stopped_early: stopped,
        })
    }

    /// Recycles one item. If Windows reports failure, the item counts as still there only when it
    /// is verifiably whole: a file that exists, a folder whose stat equals what review recorded.
    /// Anything else may have been destroyed, and the caller stops everything.
    fn recycle_step(&self, rec: &Rec) -> Step {
        let failure = match self
            .recycler
            .recycle(std::slice::from_ref(&rec.path))
            .into_iter()
            .next()
        {
            Some(Ok(bin_path)) => return Step::Moved(bin_path),
            other => other
                .and_then(Result::err)
                .unwrap_or_else(|| "no answer".into()),
        };
        let whole = if rec.stat.is_dir {
            self.facts.stat(&rec.path).as_ref() == Some(&rec.stat)
        } else {
            std::fs::symlink_metadata(&rec.path).is_ok()
        };
        if whole {
            Step::Stayed(failure)
        } else {
            Step::Stop(failure)
        }
    }

    /// Rule 5 and the rules before it, for one item, reading the disk again.
    fn check<'a>(
        &self,
        plan: &Plan<'a>,
        stayed: &HashSet<u32>,
        id: u32,
        ctx: &Ctx,
    ) -> Result<Checked<'a>, String> {
        let rec: &'a Rec = &plan.review.recs[&id];
        if let Some(l) = rec.lock {
            return Err(lock_reason(l).into());
        }
        let now = self.facts.stat(&rec.path).ok_or(GONE)?;
        if now != rec.stat || !self.same_id(rec) {
            return Err(CHANGED.into());
        }
        let (lock, admin, drive) = self.judge(&rec.path, &rec.canon, rec.why, &now, ctx);
        if let Some(l) = lock {
            return Err(lock_reason(l).into());
        }
        let twin = match rec.group {
            Some(g) => Some(self.survivor(plan, stayed, id, g)?),
            None => None,
        };
        Ok(Checked { admin, twin, drive })
    }

    fn same_id(&self, r: &Rec) -> bool {
        r.file_id.is_some() && self.facts.file_id(&r.path) == r.file_id
    }

    /// Still what review saw: same stat, same file on disk (not swapped for a look-alike).
    fn unchanged(&self, r: &Rec) -> bool {
        self.facts.stat(&r.path).as_ref() == Some(&r.stat) && self.same_id(r)
    }

    /// Rule 4: another copy of the group must stay, unchanged since review, a different file on
    /// disk (not a hard link), byte for byte the same, and not in the Recycle Bin, not in a
    /// selected folder that is about to go. Locked copies count. Returns that copy.
    fn survivor<'a>(
        &self,
        plan: &Plan<'a>,
        stayed: &HashSet<u32>,
        id: u32,
        group: u32,
    ) -> Result<&'a Rec, String> {
        let rec = &plan.review.recs[&id];
        let going: Vec<&String> = plan
            .sel
            .iter()
            .filter(|d| !stayed.contains(*d) && plan.review.recs[*d].stat.is_dir)
            .flat_map(|d| &plan.spell[d])
            .collect();
        let mut candidates = 0;
        for (oid, o) in plan
            .review
            .recs
            .iter()
            .filter(|(k, o)| **k != id && o.group == Some(group))
        {
            // Selected copies don't count, unless they were tried first and stayed.
            if plan.sel.contains(oid) && !stayed.contains(oid) {
                continue;
            }
            let os = spellings(&o.path);
            if os
                .iter()
                .any(|s| in_system_store(s) || going.iter().any(|d| is_inside(s, d)))
            {
                continue;
            }
            candidates += 1;
            if self.unchanged(o)
                && o.file_id != rec.file_id
                && self.facts.same_bytes(&rec.path, &o.path) == Ok(true)
            {
                return Ok(o);
            }
        }
        Err(if candidates == 0 { LAST_COPY } else { NO_TWIN }.into())
    }
}

mod support;
#[cfg(test)]
mod tests;
