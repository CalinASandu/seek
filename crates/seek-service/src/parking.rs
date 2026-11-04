//! Indexes stay in memory only while someone uses them. A *parked* device has no index in memory: it is
//! its saved file plus, for an own device, the journal entries that came after that file. Drive changes keep
//! going to the journal meanwhile, and the next load replays them, so an idle service holds almost nothing.
//!
//! A copy of another PC's device has no journal here: it is saved before parking, and a change for it loads it.
//!
//! Lock order: `load_lock` → `parked` → `indexes` map → a device's index `RwLock` → `journals`. Holding
//! `parked` across a replay and across an apply is what keeps a change from slipping between the two.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use seek_core::{load_index, read_index_head, Checkpoint, DeviceId, DeviceIndex, IndexHeader};
use seek_sync::{Cursor, Since, JOURNAL_MAX_BYTES};

use crate::state::{Pending, State};

/// How long an index stays in memory after its last use.
pub const IDLE_UNLOAD: Duration = Duration::from_secs(60);
/// A parked device with this much waiting in its journal is folded into its file, so that nothing it still
/// needs gets trimmed from the journal.
pub const FOLD_BYTES: usize = JOURNAL_MAX_BYTES / 2;

/// What is remembered about a device whose index is not in memory.
#[derive(Debug)]
pub struct Parked {
    /// Where the volume's change journal has been followed to; the saved file may be behind this.
    pub checkpoint: Option<Checkpoint>,
    /// Journal sequence number the saved file includes.
    pub saved_seq: u64,
}

impl Parked {
    pub fn from_head(h: &IndexHeader) -> Self {
        Self { checkpoint: h.checkpoint, saved_seq: h.sync_cursor.map_or(0, |c| c.1) }
    }
}

fn cursor_of(h: &IndexHeader) -> Option<Cursor> {
    h.sync_cursor.map(|(generation, seq)| Cursor { generation, seq })
}

impl State {
    /// Whether the device has an index, in memory or parked.
    pub fn has_index(&self, id: DeviceId) -> bool {
        self.parked.lock().unwrap().contains_key(&id) || self.indexes.read().unwrap().contains_key(&id)
    }

    /// Devices with an index, in memory or parked.
    pub fn indexed_ids(&self) -> HashSet<DeviceId> {
        let mut ids: HashSet<DeviceId> = self.parked.lock().unwrap().keys().copied().collect();
        ids.extend(self.indexes.read().unwrap().keys().copied());
        ids
    }

    /// The change journal position of a device's index.
    pub fn checkpoint_of(&self, id: DeviceId) -> Option<Checkpoint> {
        if let Some(p) = self.parked.lock().unwrap().get(&id) {
            return p.checkpoint;
        }
        self.indexes.read().unwrap().get(&id).and_then(|ix| ix.read().unwrap().checkpoint)
    }

    /// Puts a new index in memory, replacing whatever the device had.
    pub(crate) fn install_index(&self, id: DeviceId, ix: DeviceIndex) {
        let mut parked = self.parked.lock().unwrap();
        parked.remove(&id);
        self.last_used.lock().unwrap().insert(id, Instant::now());
        {
            let mut usage = self.usage.lock().unwrap();
            usage.last_by.insert(id, "a new index");
            *usage.touches.entry("a new index").or_default() += 1;
        }
        self.indexes.write().unwrap().insert(id, Arc::new(RwLock::new(ix)));
    }

    /// Forgets a device's index, in memory or parked.
    pub(crate) fn drop_index(&self, id: DeviceId) {
        let mut parked = self.parked.lock().unwrap();
        parked.remove(&id);
        self.last_used.lock().unwrap().remove(&id);
        self.indexes.write().unwrap().remove(&id);
    }

    /// The window is open: loads what a search would need and keeps it loaded a while longer.
    pub fn wake(&self) {
        let ids: Vec<DeviceId> = self.visible_devices().iter().map(|d| d.id).collect();
        self.ensure_loaded(&ids, "window showing");
    }

    /// Brings these devices' indexes into memory, drive by drive at once, and marks them as just used.
    /// A device that cannot be loaded ends up without an index and is queued for a rescan.
    pub fn ensure_loaded(&self, ids: &[DeviceId], why: &'static str) {
        let now = Instant::now();
        {
            let mut used = self.last_used.lock().unwrap();
            for id in ids {
                used.insert(*id, now);
            }
            let mut usage = self.usage.lock().unwrap();
            *usage.touches.entry(why).or_default() += 1;
            for id in ids {
                usage.last_by.insert(*id, why);
            }
        }
        let waiting = |set: &std::collections::HashMap<DeviceId, Parked>| -> Vec<DeviceId> {
            ids.iter().copied().filter(|id| set.contains_key(id)).collect()
        };
        if waiting(&self.parked.lock().unwrap()).is_empty() {
            return;
        }
        let _one_at_a_time = self.load_lock.lock().unwrap();
        let todo = waiting(&self.parked.lock().unwrap());
        let started = Instant::now();
        let loaded: Vec<_> = std::thread::scope(|s| {
            let tasks: Vec<_> = todo
                .iter()
                .map(|&id| s.spawn(move || (id, load_index(&self.index_path(id)))))
                .collect();
            tasks.into_iter().map(|t| t.join().expect("loading an index panicked")).collect()
        });
        if loaded.is_empty() {
            return;
        }
        let mut parked = self.parked.lock().unwrap();
        for (id, result) in loaded {
            // replaced or forgotten while it was being read
            let Some(p) = parked.remove(&id) else { continue };
            match result {
                Ok((head, ix)) => {
                    self.finish_load(id, head, ix, p);
                    *self.usage.lock().unwrap().loads.entry(id).or_default() += 1;
                }
                Err(e) => self.load_failed(id, e),
            }
        }
        tracing::info!("{} index(es) loaded in {} ms, for: {why}", todo.len(), started.elapsed().as_millis());
    }

    /// Replays what the saved file is missing and puts the index in memory.
    fn finish_load(&self, id: DeviceId, head: IndexHeader, mut ix: DeviceIndex, p: Parked) {
        if self.is_own_device(id) {
            let saved = cursor_of(&head).unwrap_or_else(Cursor::none);
            match self.with_journal(id, |j| j.since(&saved)) {
                Since::UpToDate => {}
                Since::Delta(changes) => {
                    ix.apply(&changes);
                    self.mark_dirty(id);
                    let count = ix.live_count() as u64;
                    if let Some(d) = self.registry.write().unwrap().get_mut(id) {
                        d.entry_count = count;
                    }
                }
                Since::Full => {
                    tracing::warn!("journal of {id} no longer reaches its saved index; rescanning");
                    self.queue_rescan(id);
                }
            }
        }
        if p.checkpoint.is_some() {
            ix.checkpoint = p.checkpoint;
        }
        self.indexes.write().unwrap().insert(id, Arc::new(RwLock::new(ix)));
    }

    fn load_failed(&self, id: DeviceId, e: seek_core::Error) {
        tracing::warn!("index of {id} unreadable ({e}); will rescan");
        let _ = std::fs::remove_file(self.index_path(id));
        self.remote_cursors.lock().unwrap().remove(&id);
        self.queue_rescan(id);
    }

    fn queue_rescan(&self, id: DeviceId) {
        self.needs_rescan.lock().unwrap().insert(id);
        if self.mounted.lock().unwrap().contains_key(&id) {
            self.rescan_pending.lock().unwrap().insert(id, Pending::Full);
        }
    }

    /// The saved file's header, if that file plus the journal fully describe the index in memory; otherwise why not.
    fn stored_head(&self, id: DeviceId) -> Result<IndexHeader, String> {
        let head = read_index_head(&self.index_path(id)).map_err(|e| format!("saved file unreadable: {e}"))?;
        if self.is_own_device(id) {
            let c = cursor_of(&head).ok_or("saved file has no journal position")?;
            let (covers, now) = self.with_journal(id, |j| (j.covers(&c), j.cursor()));
            if !covers {
                return Err(format!("journal at {}:{} does not reach the saved file at {}:{}", now.generation, now.seq, c.generation, c.seq));
            }
        } else if self.dirty.lock().unwrap().contains(&id) {
            return Err("unsaved changes".into());
        }
        Ok(head)
    }

    /// Takes indexes unused for `after` out of memory. Returns how many went.
    pub fn park_idle(&self, after: Duration) -> usize {
        let now = Instant::now();
        self.usage.lock().unwrap().idle_checks += 1;
        let held: Vec<DeviceId> = self.indexes.read().unwrap().keys().copied().collect();
        let idle: Vec<DeviceId> = {
            let mut used = self.last_used.lock().unwrap();
            held.into_iter().filter(|id| now.duration_since(*used.entry(*id).or_insert(now)) >= after).collect()
        };
        if idle.is_empty() {
            return 0;
        }
        let mut count = 0;
        let mut parked = self.parked.lock().unwrap();
        for id in idle {
            let head = self.stored_head(id).or_else(|_| {
                self.save_index(id);
                self.stored_head(id)
            });
            let head = match head {
                Ok(head) => head,
                Err(why) => {
                    if self.usage.lock().unwrap().refused.insert(id, why.clone()).as_ref() != Some(&why) {
                        tracing::warn!("index of {id} stays in memory: {why}");
                    }
                    continue;
                }
            };
            {
                let mut usage = self.usage.lock().unwrap();
                usage.refused.remove(&id);
                *usage.unloads.entry(id).or_default() += 1;
            }
            let checkpoint = self.checkpoint_in_memory(id).or(head.checkpoint);
            parked.insert(id, Parked { checkpoint, ..Parked::from_head(&head) });
            self.indexes.write().unwrap().remove(&id);
            self.dirty.lock().unwrap().remove(&id);
            count += 1;
        }
        drop(parked);
        if count > 0 {
            self.hidden_masks.lock().unwrap().clear();
            self.space_totals.lock().unwrap().clear();
            self.parked_gen.send_modify(|g| *g += 1);
            tracing::info!("{count} idle index(es) unloaded");
        }
        count
    }

    fn checkpoint_in_memory(&self, id: DeviceId) -> Option<Checkpoint> {
        self.indexes.read().unwrap().get(&id).and_then(|ix| ix.read().unwrap().checkpoint)
    }

    /// Writes parked devices with more than `limit` bytes waiting in their journal back to their files.
    /// They stay in memory until they count as idle again.
    pub fn fold_parked(&self, limit: usize) {
        let waiting: Vec<(DeviceId, u64)> =
            self.parked.lock().unwrap().iter().map(|(id, p)| (*id, p.saved_seq)).collect();
        for (id, saved) in waiting {
            if self.is_own_device(id) && self.with_journal(id, |j| j.bytes_after(saved)) > limit {
                self.ensure_loaded(&[id], "journal backlog written to file");
                self.save_index(id);
            }
        }
    }

    /// One round of idle housekeeping.
    pub fn idle_tick(&self) {
        if let Some(after) = self.cfg.idle_unload {
            self.park_idle(after);
            self.fold_parked(FOLD_BYTES);
        }
    }
}

#[cfg(test)]
mod tests {
    use seek_core::{synth_index, Change};

    use super::*;
    use crate::state::Config;
    use crate::test_support::{volume, NoVolumes};

    const CP: Checkpoint = Checkpoint { journal_id: 7, next_usn: 99 };

    fn open(dir: &std::path::Path) -> Arc<State> {
        let cfg = Config { data_dir: dir.to_path_buf(), pipe_name: String::new(), sync: None, idle_unload: None };
        State::load(cfg, Arc::new(NoVolumes)).unwrap()
    }

    /// An own device with a saved index of 100 entries.
    fn device(dir: &std::path::Path) -> (Arc<State>, DeviceId) {
        let state = open(dir);
        let id = crate::monitor::register_volume(&state, &volume());
        state.replace_index(id, synth_index(100, 1));
        state.save_index(id);
        (state, id)
    }

    fn resident(state: &State, id: DeviceId) -> bool {
        state.indexes.read().unwrap().contains_key(&id)
    }

    fn live(state: &State, id: DeviceId) -> usize {
        state.ensure_loaded(&[id], "test");
        state.indexes.read().unwrap()[&id].read().unwrap().live_count()
    }

    fn make_remote(state: &State, id: DeviceId) {
        state.registry.write().unwrap().get_mut(id).unwrap().indexed_by_host = uuid::Uuid::new_v4();
    }

    #[test]
    fn an_idle_index_leaves_memory_and_a_search_brings_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id) = device(dir.path());
        let before = live(&state, id);
        assert_eq!(state.park_idle(Duration::ZERO), 1);
        assert!(!resident(&state, id));
        assert!(state.has_index(id), "a parked device still counts as indexed");
        assert_eq!(state.snapshot("test").len(), 1);
        assert!(resident(&state, id));
        assert_eq!(live(&state, id), before);
    }

    #[test]
    fn an_index_in_use_stays() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id) = device(dir.path());
        state.ensure_loaded(&[id], "test");
        assert_eq!(state.park_idle(Duration::from_secs(60)), 0);
        assert!(resident(&state, id));
    }

    #[test]
    fn the_service_starts_with_nothing_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id) = device(dir.path());
        drop(state);
        let state = open(dir.path());
        assert!(state.indexes.read().unwrap().is_empty());
        assert!(state.has_index(id));
        assert_eq!(state.snapshot("test").len(), 1);
    }

    #[test]
    fn changes_wait_in_the_journal_while_parked() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id) = device(dir.path());
        let before = live(&state, id);
        let seq = state.journal_cursor(id).seq;
        state.park_idle(Duration::ZERO);

        assert!(state.apply_changes(id, &[Change::Delete { id: 105 }], Some(CP)).is_some());
        assert!(!resident(&state, id), "a drive change must not load the index");
        assert_eq!(state.journal_cursor(id).seq, seq + 1);
        assert_eq!(state.checkpoint_of(id), Some(CP));

        assert_eq!(live(&state, id), before - 1);
        assert_eq!(state.device(id).unwrap().entry_count, (before - 1) as u64);
        assert_eq!(state.checkpoint_of(id), Some(CP));
        assert!(state.dirty.lock().unwrap().contains(&id), "the replayed changes still need saving");
    }

    #[test]
    fn a_restart_replays_what_the_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id) = device(dir.path());
        let before = live(&state, id);
        state.park_idle(Duration::ZERO);
        state.apply_changes(id, &[Change::Delete { id: 105 }], None);
        let cursor = state.journal_cursor(id);
        drop(state);

        let state = open(dir.path());
        assert_eq!(state.journal_cursor(id), cursor, "linked PCs must not need a full copy");
        assert_eq!(live(&state, id), before - 1);
    }

    #[test]
    fn parking_needs_no_save_for_an_own_device() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id) = device(dir.path());
        let before = live(&state, id);
        state.apply_changes(id, &[Change::Delete { id: 105 }], None);
        state.mark_dirty(id);
        let saved_at = read_index_head(&state.index_path(id)).unwrap().saved_at;
        state.park_idle(Duration::ZERO);
        assert_eq!(read_index_head(&state.index_path(id)).unwrap().saved_at, saved_at);
        assert_eq!(live(&state, id), before - 1);
    }

    #[test]
    fn an_index_the_file_does_not_describe_is_saved_first() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id) = device(dir.path());
        // a new journal generation that was never saved
        state.replace_index(id, synth_index(10, 2));
        assert_eq!(state.park_idle(Duration::ZERO), 1);
        assert_eq!(live(&state, id), synth_index(10, 2).live_count());
    }

    #[test]
    fn a_copy_of_another_pc_is_saved_before_parking_and_loaded_for_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id) = device(dir.path());
        make_remote(&state, id);
        let before = live(&state, id);
        state.apply_changes(id, &[Change::Delete { id: 105 }], None);
        state.mark_dirty(id);
        state.park_idle(Duration::ZERO);
        assert!(!resident(&state, id));
        assert!(!state.dirty.lock().unwrap().contains(&id));

        assert!(state.apply_changes(id, &[Change::Delete { id: 106 }], None).is_some());
        assert!(resident(&state, id), "no journal here, so the change goes into the index");
        assert_eq!(live(&state, id), before - 2);
    }

    #[test]
    fn a_damaged_file_means_a_rescan_not_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id) = device(dir.path());
        state.park_idle(Duration::ZERO);
        let path = state.index_path(id);
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 20]).unwrap();

        assert!(state.snapshot("test").is_empty());
        assert!(!state.has_index(id));
        assert!(state.needs_rescan.lock().unwrap().contains(&id));
        assert!(!path.exists());
    }

    #[test]
    fn lost_history_keeps_the_old_index_and_queues_a_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id) = device(dir.path());
        state.mounted.lock().unwrap().insert(id, volume());
        state.park_idle(Duration::ZERO);
        state.with_journal(id, |j| j.reset());

        assert_eq!(state.snapshot("test").len(), 1);
        assert_eq!(state.rescan_pending.lock().unwrap().get(&id), Some(&Pending::Full));
    }

    #[test]
    fn a_new_index_replaces_a_parked_one() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id) = device(dir.path());
        state.park_idle(Duration::ZERO);
        state.replace_index(id, synth_index(10, 2));
        assert!(resident(&state, id));
        assert_eq!(live(&state, id), synth_index(10, 2).live_count());
    }

    #[test]
    fn a_long_backlog_is_folded_into_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id) = device(dir.path());
        let before = live(&state, id);
        state.park_idle(Duration::ZERO);
        state.apply_changes(id, &[Change::Delete { id: 105 }], None);
        state.fold_parked(usize::MAX);
        assert!(!resident(&state, id), "below the limit nothing happens");
        state.fold_parked(0);
        state.park_idle(Duration::ZERO);
        let head = read_index_head(&state.index_path(id)).unwrap();
        assert_eq!(head.sync_cursor.unwrap().1, state.journal_cursor(id).seq);
        assert_eq!(live(&state, id), before - 1);
    }

    #[test]
    fn parking_drops_what_was_computed_from_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id) = device(dir.path());
        let gen = state.parked_gen.subscribe();
        let d = state.device(id).unwrap();
        {
            let map = state.indexes.read().unwrap();
            let ix = map[&id].read().unwrap();
            state.space_totals(&d, &ix, "", None);
        }
        assert_eq!(state.space_totals.lock().unwrap().len(), 1);
        state.park_idle(Duration::ZERO);
        assert!(state.space_totals.lock().unwrap().is_empty());
        assert!(gen.has_changed().unwrap(), "connections drop their stored results");
    }

    #[test]
    fn the_report_says_where_an_index_is_and_who_used_it() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id) = device(dir.path());
        state.ensure_loaded(&[id], "search");
        let r = state.memory_report();
        let d = r.devices.iter().find(|d| d.name.contains("T:")).unwrap();
        assert!(d.in_memory && !d.on_disk_only);
        assert_eq!(d.rows, synth_index(100, 1).len() as u64);
        assert!(d.bytes > 0);
        assert_eq!(d.bytes, d.columns.iter().map(|c| c.1).sum::<u64>());
        assert_eq!(d.last_used_by.as_deref(), Some("search"));
        assert_eq!(d.idle_secs, Some(0));
        assert!(r.touches.iter().any(|(k, n)| k == "search" && *n == 1));
        assert!(r.heap_live > 0 && r.working_set > 0 && r.private_bytes > 0);

        state.park_idle(Duration::ZERO);
        let r = state.memory_report();
        let d = r.devices.iter().find(|d| d.name.contains("T:")).unwrap();
        assert!(!d.in_memory && d.on_disk_only);
        assert_eq!((d.rows, d.bytes, d.unloads), (0, 0, 1));
        assert_eq!(r.idle_checks, 1);

        state.snapshot("search");
        let r = state.memory_report();
        assert_eq!(r.devices.iter().find(|d| d.name.contains("T:")).unwrap().loads, 1);
    }

    #[test]
    fn a_refused_unload_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id) = device(dir.path());
        make_remote(&state, id);
        state.ensure_loaded(&[id], "test");
        // the file cannot be replaced while it is a directory, so saving fails too
        let path = state.index_path(id);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(state.park_idle(Duration::ZERO), 0);
        let r = state.memory_report();
        let d = r.devices.iter().find(|d| d.name.contains("T:")).unwrap();
        assert!(d.in_memory);
        assert!(d.refused.as_deref().is_some_and(|w| w.contains("unreadable")), "{:?}", d.refused);
    }

    #[test]
    fn idle_tick_does_nothing_when_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id) = device(dir.path());
        state.ensure_loaded(&[id], "test");
        state.idle_tick();
        assert!(resident(&state, id));
    }
}
