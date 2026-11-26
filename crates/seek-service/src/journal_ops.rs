//! Index changes that linked PCs must hear about: every batch applied to an own device's index is recorded
//! in that device's change journal, and replacing an index starts a new journal generation.
//!
//! Lock order: `parked` → `indexes` map → a device's index `RwLock` → `journals`. Holding the map lock across
//! an apply/replace keeps a journal cursor and the index contents it describes consistent.

use std::path::PathBuf;

use seek_core::{Change, Checkpoint, DeviceId, DeviceIndex};
use seek_sync::{Cursor, DeviceJournal, JOURNAL_MAX_BYTES};

use crate::state::State;

impl State {
    fn journal_path(&self, id: DeviceId) -> PathBuf {
        self.cfg.data_dir.join("journal").join(format!("{id}.log"))
    }

    /// Runs `f` on a device's journal, opening it on first use.
    pub(crate) fn with_journal<T>(
        &self,
        id: DeviceId,
        f: impl FnOnce(&mut DeviceJournal) -> T,
    ) -> T {
        let mut journals = self.journals.lock().unwrap();
        let j = journals
            .entry(id)
            .or_insert_with(|| DeviceJournal::open(self.journal_path(id), JOURNAL_MAX_BYTES));
        f(j)
    }

    pub fn is_own_device(&self, id: DeviceId) -> bool {
        let reg = self.registry.read().unwrap();
        reg.get(id)
            .is_some_and(|d| d.indexed_by_host == reg.host_id)
    }

    /// Applies `changes` to a device's index (and, for an own device, journals them). A parked own device
    /// only journals them; its count then stays as it was until the next load. Returns the live entry
    /// count, or `None` when the device has no index.
    pub fn apply_changes(
        &self,
        id: DeviceId,
        changes: &[Change],
        checkpoint: Option<Checkpoint>,
    ) -> Option<u64> {
        let own = self.is_own_device(id);
        let mut parked = self.parked.lock().unwrap();
        if let Some(p) = parked.get_mut(&id) {
            if own {
                if checkpoint.is_some() {
                    p.checkpoint = checkpoint;
                }
                self.with_journal(id, |j| j.record(changes));
                self.own_index_changed();
                return self.device(id).map(|d| d.entry_count);
            }
            // another PC's device has no journal here
            drop(parked);
            self.ensure_loaded(&[id], "change from a linked PC");
            parked = self.parked.lock().unwrap();
        }
        let _in_memory_until_applied = parked;
        let map = self.indexes.read().unwrap();
        let ix = map.get(&id)?;
        let mut ix = ix.write().unwrap();
        ix.apply(changes);
        if checkpoint.is_some() {
            ix.checkpoint = checkpoint;
        }
        if own {
            self.with_journal(id, |j| j.record(changes));
            self.own_index_changed();
        }
        Some(ix.live_count() as u64)
    }

    /// Installs a new index for a device (full scan, pushed share index). For an own device the journal
    /// starts a new generation, so linked PCs fetch a full copy.
    pub fn replace_index(&self, id: DeviceId, ix: DeviceIndex) {
        let own = self.is_own_device(id);
        self.install_index(id, ix);
        if own {
            self.with_journal(id, |j| j.reset());
            self.own_index_changed();
        }
    }

    /// Tells the hub session (if any) that an own index changed.
    fn own_index_changed(&self) {
        if let Some(rt) = self.peers_rt.get() {
            rt.index_changed();
        }
    }

    /// A consistent `(cursor, compacted copy)` of an own device, for a full copy to a linked PC.
    pub fn own_snapshot(&self, id: DeviceId) -> Option<(Cursor, DeviceIndex)> {
        self.ensure_loaded(&[id], "full copy for a linked PC or the hub");
        let (cursor, mut copy) = {
            let map = self.indexes.read().unwrap();
            let ix = map.get(&id)?.read().unwrap();
            let cursor = self.with_journal(id, |j| j.cursor());
            (cursor, ix.clone())
        };
        copy.compact();
        Some((cursor, copy))
    }

    pub fn journal_cursor(&self, id: DeviceId) -> Cursor {
        self.with_journal(id, |j| j.cursor())
    }

    pub(crate) fn remove_journal(&self, id: DeviceId) {
        self.journals.lock().unwrap().remove(&id);
        let _ = std::fs::remove_file(self.journal_path(id));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use seek_core::synth_index;

    use super::*;
    use crate::state::Config;
    use crate::test_support::{volume, NoVolumes};

    #[test]
    fn applied_changes_are_journaled_and_replace_starts_a_new_generation() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            data_dir: dir.path().to_path_buf(),
            pipe_name: String::new(),
            sync: None,
            idle_unload: None,
        };
        let state = State::load(cfg, Arc::new(NoVolumes)).unwrap();
        let id = crate::monitor::register_volume(&state, &volume());
        state.replace_index(id, synth_index(1_000, 1));
        let c0 = state.journal_cursor(id);
        assert_eq!(c0.seq, 0);

        assert_eq!(
            state.apply_changes(id, &[Change::Delete { id: 105 }], None),
            Some(999)
        );
        let c1 = state.journal_cursor(id);
        assert_eq!((c1.generation, c1.seq), (c0.generation, 1));

        let (cursor, copy) = state.own_snapshot(id).unwrap();
        assert_eq!(cursor, c1);
        assert_eq!(copy.live_count(), 999);
        assert_eq!(copy.len(), 999, "the copy is compacted");

        state.replace_index(id, synth_index(10, 2));
        let c2 = state.journal_cursor(id);
        assert_ne!(c2.generation, c0.generation);
        assert_eq!(c2.seq, 0);
    }

    #[test]
    fn remote_devices_are_not_journaled() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            data_dir: dir.path().to_path_buf(),
            pipe_name: String::new(),
            sync: None,
            idle_unload: None,
        };
        let state = State::load(cfg, Arc::new(NoVolumes)).unwrap();
        let id = crate::monitor::register_volume(&state, &volume());
        state
            .registry
            .write()
            .unwrap()
            .get_mut(id)
            .unwrap()
            .indexed_by_host = uuid::Uuid::new_v4();
        state.replace_index(id, synth_index(100, 1));
        state.apply_changes(id, &[Change::Delete { id: 105 }], None);
        assert!(state.journals.lock().unwrap().is_empty());
    }

    #[test]
    fn restart_keeps_the_generation_only_when_the_saved_index_matches() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = || Config {
            data_dir: dir.path().to_path_buf(),
            pipe_name: String::new(),
            sync: None,
            idle_unload: None,
        };
        let state = State::load(cfg(), Arc::new(NoVolumes)).unwrap();
        let id = crate::monitor::register_volume(&state, &volume());
        state.replace_index(id, synth_index(100, 1));
        state.apply_changes(id, &[Change::Delete { id: 105 }], None);
        state.save_index(id);
        let saved = state.journal_cursor(id);
        // a change after the last save is missing from the file, but the journal still has it…
        state.apply_changes(id, &[Change::Delete { id: 106 }], None);
        let newest = state.journal_cursor(id);
        drop(state);
        let state = State::load(cfg(), Arc::new(NoVolumes)).unwrap();
        // …so the next load replays it and linked PCs keep their deltas
        assert_eq!(state.journal_cursor(id), newest);
        assert_ne!(saved, newest);

        // a journal that does not reach the file means a new generation
        state.with_journal(id, |j| j.reset());
        let reset = state.journal_cursor(id);
        drop(state);
        let state = State::load(cfg(), Arc::new(NoVolumes)).unwrap();
        assert_ne!(state.journal_cursor(id).generation, reset.generation);

        state.save_index(id);
        let saved = state.journal_cursor(id);
        drop(state);
        let state = State::load(cfg(), Arc::new(NoVolumes)).unwrap();
        assert_eq!(state.journal_cursor(id), saved);
    }
}
