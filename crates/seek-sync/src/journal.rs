//! Per-device change journal: every batch of changes applied to an own device's index gets the next sequence
//! number, so a copy on another PC can be brought up to date with only what it is missing.
//!
//! File (`journal\<device>.log`): `MAGIC | generation (16 bytes) | records`, each record
//! `u32 LE length | postcard((seq, Vec<Change>))`. A torn last record (crash mid-append) is dropped on open;
//! anything unreadable starts a new generation, which makes every copy fetch a full index once.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;

use seek_core::Change;
use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 8] = b"SEEKJRN1";
const HEADER_LEN: usize = 8 + 16;

/// Default history kept per device.
pub const JOURNAL_MAX_BYTES: usize = 50 * 1024 * 1024;

/// Position of a copy: the journal generation and the last sequence number applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cursor {
    pub generation: uuid::Uuid,
    pub seq: u64,
}

impl Cursor {
    /// A cursor no journal matches: asks for a full copy.
    pub fn none() -> Self {
        Self { generation: uuid::Uuid::nil(), seq: 0 }
    }
}

/// What a copy at some cursor needs.
#[derive(Debug, PartialEq)]
pub enum Since {
    UpToDate,
    Delta(Vec<Change>),
    Full,
}

struct Batch {
    seq: u64,
    changes: Vec<Change>,
    size: usize,
}

pub struct DeviceJournal {
    generation: uuid::Uuid,
    /// Sequence number of the oldest kept batch (`last_seq + 1` when none is kept).
    first_seq: u64,
    last_seq: u64,
    batches: VecDeque<Batch>,
    bytes: usize,
    max_bytes: usize,
    path: Option<PathBuf>,
    file_bytes: usize,
}

impl DeviceJournal {
    /// A journal that lives only in memory (tests, or when the file cannot be written).
    pub fn in_memory(max_bytes: usize) -> Self {
        Self {
            generation: uuid::Uuid::new_v4(),
            first_seq: 1,
            last_seq: 0,
            batches: VecDeque::new(),
            bytes: 0,
            max_bytes,
            path: None,
            file_bytes: 0,
        }
    }

    /// Loads the journal at `path`; a missing or unreadable file starts a new generation.
    pub fn open(path: PathBuf, max_bytes: usize) -> Self {
        let mut j = Self { path: Some(path.clone()), ..Self::in_memory(max_bytes) };
        match std::fs::read(&path) {
            Ok(bytes) if bytes.len() >= HEADER_LEN && &bytes[..8] == MAGIC => {
                let mut gen = [0u8; 16];
                gen.copy_from_slice(&bytes[8..HEADER_LEN]);
                j.generation = uuid::Uuid::from_bytes(gen);
                let mut pos = HEADER_LEN;
                while pos + 4 <= bytes.len() {
                    let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
                    let Some(rec) = bytes.get(pos + 4..pos + 4 + len) else { break };
                    let Ok((seq, changes)) = postcard::from_bytes::<(u64, Vec<Change>)>(rec) else { break };
                    if j.last_seq != 0 && seq != j.last_seq + 1 {
                        break;
                    }
                    j.last_seq = seq;
                    j.push(seq, changes, len + 4);
                    pos += 4 + len;
                }
                j.first_seq = j.batches.front().map_or(j.last_seq + 1, |b| b.seq);
                j.trim();
                if pos != bytes.len() {
                    // torn or damaged tail: keep what was readable
                    j.rewrite();
                } else {
                    j.file_bytes = pos;
                }
            }
            Ok(_) => {
                tracing::warn!("journal {} unreadable; starting a new generation", path.display());
                j.rewrite();
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => j.rewrite(),
            Err(e) => {
                tracing::warn!("journal {} unreadable ({e}); starting a new generation", path.display());
                j.rewrite();
            }
        }
        j
    }

    pub fn cursor(&self) -> Cursor {
        Cursor { generation: self.generation, seq: self.last_seq }
    }

    /// Records one applied batch of changes.
    pub fn record(&mut self, changes: &[Change]) {
        self.last_seq += 1;
        let rec = postcard::to_stdvec(&(self.last_seq, changes)).expect("changes serialize");
        self.append(&rec);
        self.push(self.last_seq, changes.to_vec(), rec.len() + 4);
        self.trim();
        if self.file_bytes > 2 * self.max_bytes.max(1) {
            self.rewrite();
        }
    }

    /// The index was replaced wholesale (full scan, reformat): copies need a full copy.
    pub fn reset(&mut self) {
        self.generation = uuid::Uuid::new_v4();
        self.last_seq = 0;
        self.first_seq = 1;
        self.batches.clear();
        self.bytes = 0;
        self.rewrite();
    }

    pub fn since(&self, c: &Cursor) -> Since {
        if c.generation != self.generation || c.seq > self.last_seq {
            return Since::Full;
        }
        if c.seq == self.last_seq {
            return Since::UpToDate;
        }
        if self.first_seq > c.seq + 1 {
            return Since::Full;
        }
        Since::Delta(
            self.batches.iter().filter(|b| b.seq > c.seq).flat_map(|b| b.changes.iter().cloned()).collect(),
        )
    }

    fn push(&mut self, seq: u64, changes: Vec<Change>, size: usize) {
        self.bytes += size;
        self.batches.push_back(Batch { seq, changes, size });
    }

    /// Drops the oldest batches beyond `max_bytes` (always keeps the newest one).
    fn trim(&mut self) {
        while self.bytes > self.max_bytes && self.batches.len() > 1 {
            let b = self.batches.pop_front().unwrap();
            self.bytes -= b.size;
        }
        self.first_seq = self.batches.front().map_or(self.last_seq + 1, |b| b.seq);
    }

    fn append(&mut self, rec: &[u8]) {
        let Some(path) = &self.path else { return };
        let res = std::fs::OpenOptions::new().append(true).open(path).and_then(|mut f| {
            f.write_all(&(rec.len() as u32).to_le_bytes())?;
            f.write_all(rec)
        });
        match res {
            Ok(()) => self.file_bytes += rec.len() + 4,
            Err(e) => tracing::warn!("appending to journal {} failed: {e}", path.display()),
        }
    }

    /// Writes header + kept batches to a temporary file and renames it over the journal.
    fn rewrite(&mut self) {
        let Some(path) = &self.path else { return };
        let mut out = Vec::with_capacity(HEADER_LEN + self.bytes);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(self.generation.as_bytes());
        for b in &self.batches {
            let rec = postcard::to_stdvec(&(b.seq, &b.changes)).expect("changes serialize");
            out.extend_from_slice(&(rec.len() as u32).to_le_bytes());
            out.extend_from_slice(&rec);
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::write(&tmp, &out).and_then(|_| std::fs::rename(&tmp, path)) {
            Ok(()) => self.file_bytes = out.len(),
            Err(e) => tracing::warn!("writing journal {} failed: {e}", path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seek_core::RawEntry;

    fn ch(id: u64) -> Vec<Change> {
        vec![Change::Upsert(RawEntry {
            id,
            parent_id: 0,
            name: format!("f{id}"),
            size: id,
            modified: 0,
            created: 0,
            attrs: 0,
        })]
    }

    #[test]
    fn delta_full_and_up_to_date() {
        let mut j = DeviceJournal::in_memory(JOURNAL_MAX_BYTES);
        let start = j.cursor();
        assert_eq!(start.seq, 0);
        assert_eq!(j.since(&start), Since::UpToDate);
        j.record(&ch(1));
        j.record(&ch(2));
        assert_eq!(j.since(&start), Since::Delta([ch(1), ch(2)].concat()));
        assert_eq!(j.since(&Cursor { seq: 1, ..start }), Since::Delta(ch(2)));
        assert_eq!(j.since(&j.cursor()), Since::UpToDate);
        assert_eq!(j.since(&Cursor { generation: uuid::Uuid::new_v4(), seq: 1 }), Since::Full);
        assert_eq!(j.since(&Cursor::none()), Since::Full);
        assert_eq!(j.since(&Cursor { seq: 99, ..j.cursor() }), Since::Full);
        j.reset();
        assert_ne!(j.cursor().generation, start.generation);
        assert_eq!(j.cursor().seq, 0);
        assert_eq!(j.since(&start), Since::Full);
    }

    #[test]
    fn trimmed_history_means_full_copy() {
        let mut j = DeviceJournal::in_memory(300);
        let start = j.cursor();
        for i in 0..50 {
            j.record(&ch(i));
        }
        assert_eq!(j.since(&start), Since::Full);
        let recent = Cursor { seq: j.cursor().seq - 1, ..j.cursor() };
        assert_eq!(j.since(&recent), Since::Delta(ch(49)));
    }

    #[test]
    fn survives_restart_and_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.log");
        let mut j = DeviceJournal::open(p.clone(), JOURNAL_MAX_BYTES);
        let start = j.cursor();
        j.record(&ch(1));
        j.record(&ch(2));
        drop(j);
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(&[9, 0, 0]).unwrap();
        let mut j = DeviceJournal::open(p.clone(), JOURNAL_MAX_BYTES);
        assert_eq!(j.cursor(), Cursor { seq: 2, ..start });
        assert_eq!(j.since(&start), Since::Delta([ch(1), ch(2)].concat()));
        // appends after a repaired tail are readable again
        j.record(&ch(3));
        let j = DeviceJournal::open(p, JOURNAL_MAX_BYTES);
        assert_eq!(j.cursor(), Cursor { seq: 3, ..start });
    }

    #[test]
    fn trimmed_file_is_rewritten_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.log");
        let mut j = DeviceJournal::open(p.clone(), 200);
        for i in 0..100 {
            j.record(&ch(i));
        }
        let c = j.cursor();
        drop(j);
        assert!(std::fs::metadata(&p).unwrap().len() < 1000);
        let j = DeviceJournal::open(p, 200);
        assert_eq!(j.cursor(), c);
        assert_eq!(j.since(&Cursor { seq: c.seq - 1, ..c }), Since::Delta(ch(99)));
    }

    #[test]
    fn corrupt_file_starts_a_new_generation() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.log");
        std::fs::write(&p, b"garbage").unwrap();
        let j = DeviceJournal::open(p.clone(), JOURNAL_MAX_BYTES);
        assert_eq!(j.cursor().seq, 0);
        let again = DeviceJournal::open(p, JOURNAL_MAX_BYTES);
        assert_eq!(again.cursor(), j.cursor());
    }
}
