//! Deterministic synthetic indexes for tests and benchmarks.

use crate::index::{DeviceIndex, NO_PARENT};
use crate::model::ATTR_DIRECTORY;

const WORDS: &[&str] = &[
    "report",
    "invoice",
    "photo",
    "backup",
    "notes",
    "project",
    "budget",
    "meeting",
    "summary",
    "draft",
    "final",
    "archive",
    "image",
    "document",
    "screenshot",
    "holiday",
    "family",
    "music",
    "video",
    "setup",
    "config",
    "readme",
    "main",
    "index",
    "test",
    "data",
    "export",
    "import",
    "release",
    "build",
    "script",
    "module",
    "package",
    "cache",
    "temp",
    "log",
    "session",
    "profile",
    "template",
    "resume",
    "contract",
    "receipt",
    "ticket",
    "slides",
    "lecture",
    "thesis",
    "paper",
    "article",
    "recipe",
    "playlist",
    "wallpaper",
    "icon",
    "font",
    "driver",
    "update",
    "install",
    "manual",
    "guide",
    "sample",
    "demo",
];

const EXTS: &[&str] = &[
    "txt", "pdf", "jpg", "png", "docx", "xlsx", "rs", "toml", "json", "md", "dll", "exe", "mp3",
    "mp4", "zip", "log", "tmp", "cs", "js", "html", "css", "bmp", "gif", "pptx", "csv",
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        ((self.next() >> 11) % n as u64) as usize
    }
}

/// `n` live rows (folders roughly 1 in 12), realistic names and a tree a few levels deep.
pub fn synth_index(n: usize, seed: u64) -> DeviceIndex {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut ix = DeviceIndex::with_capacity(5, n);
    let mut dirs: Vec<u32> = Vec::new();
    let mut name = String::with_capacity(64);
    for i in 0..n {
        let id = 100 + i as u64;
        let is_dir = dirs.is_empty() || rng.below(12) == 0;
        let parent = if dirs.is_empty() {
            NO_PARENT
        } else if rng.below(8) == 0 {
            // shallow placement keeps trees from degenerating into chains
            dirs[rng.below(dirs.len().min(64))]
        } else {
            dirs[rng.below(dirs.len())]
        };
        name.clear();
        name.push_str(WORDS[rng.below(WORDS.len())]);
        if is_dir {
            if rng.below(3) == 0 {
                name.push(' ');
                name.push_str(WORDS[rng.below(WORDS.len())]);
            }
        } else {
            match rng.below(4) {
                0 => {}
                1 => {
                    name.push('_');
                    name.push_str(&(rng.below(1000)).to_string());
                }
                2 => {
                    name.push(' ');
                    name.push_str(WORDS[rng.below(WORDS.len())]);
                }
                _ => {
                    name.push_str("-v");
                    name.push_str(&(rng.below(20)).to_string());
                }
            }
            if rng.below(7) == 0 {
                name.make_ascii_uppercase();
            }
            name.push('.');
            name.push_str(EXTS[rng.below(EXTS.len())]);
        }
        let size = if is_dir {
            0
        } else {
            rng.next() % (1u64 << (4 + rng.below(28)))
        };
        let modified = 1_400_000_000_000 + (rng.next() % 400_000_000_000) as i64;
        let attrs = if is_dir { ATTR_DIRECTORY } else { 0 };
        let row = ix.push_row(
            id,
            parent,
            &name,
            size,
            modified,
            modified - 86_400_000,
            attrs,
        );
        if is_dir {
            dirs.push(row);
        }
    }
    ix
}
