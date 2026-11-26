//! Per-device change journal: every batch of changes applied to an own device's index gets the next sequence
//! number, so a copy on another PC can be brought up to date with only what it is missing.
//!
//! File (`journal\<device>.log`): `MAGIC | generation (16 bytes) | records`, each record
//! `u32 LE length | postcard((seq, Vec<Change>))`. A torn last record (crash mid-append) is dropped on open;
//! anything unreadable starts a new generation, which makes every copy fetch a full index once.
//!
//! The history stays in the file: memory holds only where each batch is, and a batch is read back when a
//! linked PC asks for it. A batch the file refused is the exception and stays in memory.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

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

enum Stored {
    /// Offset of the record's body in the journal file.
    File(u64),
    Memory(Vec<Change>),
}

struct Batch {
    seq: u64,
    stored: Stored,
    /// Bytes of the whole record, length prefix included.
    size: usize,
}

/// Reads record bodies from the journal file; seeks only when they are not next to each other.
struct RecordReader<'a> {
    path: &'a Path,
    file: Option<BufReader<File>>,
    at: u64,
    buf: Vec<u8>,
}

impl<'a> RecordReader<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, file: None, at: 0, buf: Vec::new() }
    }

    fn body(&mut self, pos: u64, len: usize) -> io::Result<&[u8]> {
        if self.file.is_none() {
            self.file = Some(BufReader::new(File::open(self.path)?));
            self.at = 0;
        }
        let f = self.file.as_mut().expect("just opened");
        if self.at != pos {
            f.seek(SeekFrom::Start(pos))?;
        }
        self.buf.resize(len, 0);
        f.read_exact(&mut self.buf)?;
        self.at = pos + len as u64;
        Ok(&self.buf)
    }
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
        let empty = || Self { path: Some(path.clone()), ..Self::in_memory(max_bytes) };
        let mut j = empty();
        match j.read_file(&path) {
            Ok(Some((end, clean))) => {
                j.first_seq = j.batches.front().map_or(j.last_seq + 1, |b| b.seq);
                j.trim();
                if clean {
                    j.file_bytes = end as usize;
                } else {
                    // torn or damaged tail: keep what was readable
                    j.rewrite();
                }
            }
            Ok(None) => {
                tracing::warn!("journal {} unreadable; starting a new generation", path.display());
                j = empty();
                j.rewrite();
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => j.rewrite(),
            Err(e) => {
                tracing::warn!("journal {} unreadable ({e}); starting a new generation", path.display());
                j = empty();
                j.rewrite();
            }
        }
        j
    }

    /// Walks the file's records, noting where each one is. `None` when it is not a journal; otherwise
    /// the offset after the last good record and whether that is the end of the file.
    fn read_file(&mut self, path: &Path) -> io::Result<Option<(u64, bool)>> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if len < HEADER_LEN as u64 {
            return Ok(None);
        }
        let mut r = BufReader::new(file);
        let mut head = [0u8; HEADER_LEN];
        r.read_exact(&mut head)?;
        if &head[..8] != MAGIC {
            return Ok(None);
        }
        let mut gen = [0u8; 16];
        gen.copy_from_slice(&head[8..]);
        self.generation = uuid::Uuid::from_bytes(gen);
        let mut pos = HEADER_LEN as u64;
        let mut rec = Vec::new();
        while pos + 4 <= len {
            let mut n = [0u8; 4];
            r.read_exact(&mut n)?;
            let n = u32::from_le_bytes(n) as usize;
            if pos + 4 + n as u64 > len {
                break;
            }
            rec.resize(n, 0);
            r.read_exact(&mut rec)?;
            let Ok((seq, _)) = postcard::from_bytes::<(u64, Vec<Change>)>(&rec) else { break };
            if self.last_seq != 0 && seq != self.last_seq + 1 {
                break;
            }
            self.last_seq = seq;
            self.push(seq, Stored::File(pos + 4), n + 4);
            pos += 4 + n as u64;
        }
        Ok(Some((pos, pos == len)))
    }

    pub fn cursor(&self) -> Cursor {
        Cursor { generation: self.generation, seq: self.last_seq }
    }

    /// Records one applied batch of changes.
    pub fn record(&mut self, changes: &[Change]) {
        self.last_seq += 1;
        let rec = postcard::to_stdvec(&(self.last_seq, changes)).expect("changes serialize");
        let stored = match self.append(&rec) {
            Some(pos) => Stored::File(pos),
            None => Stored::Memory(changes.to_vec()),
        };
        self.push(self.last_seq, stored, rec.len() + 4);
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
        match self.changes_after(c.seq) {
            Ok(changes) => Since::Delta(changes),
            Err(e) => {
                // a full copy is slower but always right
                tracing::warn!("reading journal history failed: {e}");
                Since::Full
            }
        }
    }

    /// Approximate heap bytes held: the list of batches, and any batch the file refused.
    pub fn heap_bytes(&self) -> usize {
        let kept: usize = self
            .batches
            .iter()
            .map(|b| match &b.stored {
                Stored::File(_) => 0,
                Stored::Memory(c) => c.capacity() * std::mem::size_of::<Change>() + b.size,
            })
            .sum();
        self.batches.capacity() * std::mem::size_of::<Batch>() + kept
    }

    /// Whether everything after `c` is still kept, so `since(c)` can give a delta.
    pub fn covers(&self, c: &Cursor) -> bool {
        c.generation == self.generation && c.seq <= self.last_seq && self.first_seq <= c.seq + 1
    }

    /// Size of the history after sequence number `seq`.
    pub fn bytes_after(&self, seq: u64) -> usize {
        self.batches.iter().rev().take_while(|b| b.seq > seq).map(|b| b.size).sum()
    }

    fn changes_after(&self, seq: u64) -> io::Result<Vec<Change>> {
        let path = self.path.as_deref().unwrap_or(Path::new(""));
        let mut reader = RecordReader::new(path);
        let mut out = Vec::new();
        for b in self.batches.iter().filter(|b| b.seq > seq) {
            match &b.stored {
                Stored::Memory(changes) => out.extend(changes.iter().cloned()),
                Stored::File(pos) => {
                    let body = reader.body(*pos, b.size - 4)?;
                    let (got, changes) = postcard::from_bytes::<(u64, Vec<Change>)>(body)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    if got != b.seq {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "batch out of place"));
                    }
                    out.extend(changes);
                }
            }
        }
        Ok(out)
    }

    #[cfg(test)]
    fn batches_in_memory(&self) -> usize {
        self.batches.iter().filter(|b| matches!(b.stored, Stored::Memory(_))).count()
    }

    fn push(&mut self, seq: u64, stored: Stored, size: usize) {
        self.bytes += size;
        self.batches.push_back(Batch { seq, stored, size });
    }

    /// Drops the oldest batches beyond `max_bytes` (always keeps the newest one).
    fn trim(&mut self) {
        while self.bytes > self.max_bytes && self.batches.len() > 1 {
            let b = self.batches.pop_front().unwrap();
            self.bytes -= b.size;
        }
        self.first_seq = self.batches.front().map_or(self.last_seq + 1, |b| b.seq);
    }

    /// Appends a record; returns the offset of its body, or `None` when it is not in the file.
    fn append(&mut self, rec: &[u8]) -> Option<u64> {
        let path = self.path.as_ref()?;
        let res = std::fs::OpenOptions::new().append(true).open(path).and_then(|mut f| {
            let start = f.seek(SeekFrom::End(0))?;
            f.write_all(&(rec.len() as u32).to_le_bytes())?;
            f.write_all(rec)?;
            Ok(start)
        });
        match res {
            Ok(start) => {
                self.file_bytes = start as usize + rec.len() + 4;
                Some(start + 4)
            }
            Err(e) => {
                tracing::warn!("appending to journal {} failed: {e}", path.display());
                None
            }
        }
    }

    /// Writes header + kept batches to a temporary file and renames it over the journal.
    fn rewrite(&mut self) {
        let Some(path) = self.path.clone() else { return };
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match self.write_kept(&path, &tmp).and_then(|kept| std::fs::rename(&tmp, &path).map(|_| kept)) {
            Ok((positions, end)) => {
                for (b, pos) in self.batches.iter_mut().zip(positions) {
                    b.stored = Stored::File(pos);
                }
                self.file_bytes = end;
            }
            Err(e) => tracing::warn!("writing journal {} failed: {e}", path.display()),
        }
    }

    /// Copies the kept batches to `tmp`, record by record; returns their new offsets and the file's size.
    fn write_kept(&self, path: &Path, tmp: &Path) -> io::Result<(Vec<u64>, usize)> {
        let mut out = BufWriter::new(File::create(tmp)?);
        out.write_all(MAGIC)?;
        out.write_all(self.generation.as_bytes())?;
        let mut reader = RecordReader::new(path);
        let mut positions = Vec::with_capacity(self.batches.len());
        let mut pos = HEADER_LEN;
        for b in &self.batches {
            let packed;
            let rec: &[u8] = match &b.stored {
                Stored::File(at) => reader.body(*at, b.size - 4)?,
                Stored::Memory(changes) => {
                    packed = postcard::to_stdvec(&(b.seq, changes)).expect("changes serialize");
                    &packed
                }
            };
            out.write_all(&(rec.len() as u32).to_le_bytes())?;
            out.write_all(rec)?;
            positions.push(pos as u64 + 4);
            pos += 4 + rec.len();
        }
        out.flush()?;
        Ok((positions, pos))
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
    fn history_stays_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.log");
        let mut j = DeviceJournal::open(p.clone(), JOURNAL_MAX_BYTES);
        let start = j.cursor();
        j.record(&ch(1));
        j.record(&ch(2));
        assert_eq!(j.batches_in_memory(), 0);
        assert_eq!(j.since(&start), Since::Delta([ch(1), ch(2)].concat()));
        drop(j);
        let mut j = DeviceJournal::open(p, JOURNAL_MAX_BYTES);
        assert_eq!(j.batches_in_memory(), 0);
        assert_eq!(j.since(&start), Since::Delta([ch(1), ch(2)].concat()));
        j.record(&ch(3));
        assert_eq!(j.since(&Cursor { seq: 2, ..start }), Since::Delta(ch(3)));
    }

    #[test]
    fn rewriting_the_file_keeps_history_readable() {
        let dir = tempfile::tempdir().unwrap();
        let mut j = DeviceJournal::open(dir.path().join("d.log"), 200);
        for i in 0..100 {
            j.record(&ch(i));
        }
        assert_eq!(j.batches_in_memory(), 0);
        let c = j.cursor();
        assert_eq!(j.since(&Cursor { seq: c.seq - 2, ..c }), Since::Delta([ch(98), ch(99)].concat()));
    }

    #[test]
    fn knows_what_it_still_covers() {
        let mut j = DeviceJournal::in_memory(300);
        let start = j.cursor();
        assert!(j.covers(&start));
        j.record(&ch(1));
        assert!(j.covers(&start));
        assert!(j.bytes_after(0) > 0);
        assert_eq!(j.bytes_after(1), 0);
        assert!(!j.covers(&Cursor { seq: 9, ..start }));
        assert!(!j.covers(&Cursor::none()));
        for i in 0..50 {
            j.record(&ch(i));
        }
        assert!(!j.covers(&start), "older history was trimmed");
    }

    #[test]
    fn missing_history_means_full_copy() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.log");
        let mut j = DeviceJournal::open(p.clone(), JOURNAL_MAX_BYTES);
        let start = j.cursor();
        j.record(&ch(1));
        std::fs::remove_file(&p).unwrap();
        assert_eq!(j.since(&start), Since::Full);
    }

    #[test]
    fn a_batch_the_file_refused_is_kept_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.log");
        let mut j = DeviceJournal::open(p.clone(), JOURNAL_MAX_BYTES);
        j.record(&ch(1));
        let c = j.cursor();
        std::fs::remove_file(&p).unwrap();
        j.record(&ch(2));
        assert_eq!(j.batches_in_memory(), 1);
        assert_eq!(j.since(&c), Since::Delta(ch(2)));
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
