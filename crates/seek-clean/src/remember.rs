//! What duplicate scans have already read, so the next scan only reads files that changed.
//! One line per file: size, modified (ms), head-and-tail hash, full hash, path. Loaded for a
//! scan and dropped after it; nothing stays in memory between scans.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Known {
    pub size: u64,
    pub modified: i64,
    pub ends: blake3::Hash,
    pub full: blake3::Hash,
}

/// Keyed by the canonical (lowercase) path.
#[derive(Default)]
pub struct Remembered(HashMap<String, Known>);

/// `%LOCALAPPDATA%\Seek\dupe-prints.txt`.
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Seek").join("dupe-prints.txt")
}

/// Bytes the file takes; 0 when there is none.
pub fn bytes(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// Forgets everything.
pub fn forget(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

impl Remembered {
    /// A missing or unreadable file is empty; a line that doesn't parse is skipped.
    pub fn load(path: &Path) -> Self {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        Remembered(text.lines().filter_map(parse).collect())
    }

    /// Only while the file still has this size and modified time.
    pub fn get(&self, canon: &str, size: u64, modified: i64) -> Option<Known> {
        self.0.get(canon).copied().filter(|k| k.size == size && k.modified == modified)
    }

    pub fn put(&mut self, canon: String, known: Known) {
        self.0.insert(canon, known);
    }

    /// Writes a temp file, then puts it in place, so a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut out = String::new();
        for (p, k) in &self.0 {
            out += &format!("{}\t{}\t{}\t{}\t{}\n", k.size, k.modified, k.ends.to_hex(), k.full.to_hex(), p);
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(tmp, path)
    }
}

fn parse(line: &str) -> Option<(String, Known)> {
    let mut f = line.splitn(5, '\t');
    let known = Known {
        size: f.next()?.parse().ok()?,
        modified: f.next()?.parse().ok()?,
        ends: blake3::Hash::from_hex(f.next()?).ok()?,
        full: blake3::Hash::from_hex(f.next()?).ok()?,
    };
    Some((f.next()?.to_string(), known))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_loads_and_only_matches_an_unchanged_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Seek").join("p.txt");
        let k = Known { size: 5, modified: 7, ends: blake3::hash(b"e"), full: blake3::hash(b"f") };
        let mut r = Remembered::default();
        r.put(r"c:\a\b.bin".into(), k);
        r.save(&path).unwrap();
        std::fs::write(dir.path().join("Seek").join("junk"), "").unwrap();
        let back = Remembered::load(&path);
        assert_eq!(back.get(r"c:\a\b.bin", 5, 7), Some(k));
        assert_eq!(back.get(r"c:\a\b.bin", 5, 8), None, "modified since");
        assert_eq!(back.get(r"c:\a\b.bin", 6, 7), None, "size changed");
        assert!(bytes(&path) > 0);
        forget(&path).unwrap();
        assert_eq!(bytes(&path), 0);
        forget(&path).unwrap();
    }

    #[test]
    fn a_bad_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.txt");
        let good = format!("1\t2\t{}\t{}\tc:\\x\n", blake3::hash(b"a").to_hex(), blake3::hash(b"b").to_hex());
        std::fs::write(&path, format!("garbage\n1\t2\tnothex\tnothex\tc:\\y\n{good}")).unwrap();
        let r = Remembered::load(&path);
        assert!(r.get(r"c:\x", 1, 2).is_some());
        assert!(r.get(r"c:\y", 1, 2).is_none());
    }
}
