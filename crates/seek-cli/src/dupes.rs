//! `seek dupes`: runs the app's duplicate scan on this PC's online drives and says where the time went.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::bail;
use seek_clean::dupes::{self, ScanEvent, SizeBatch, SizeFile, MIN_DUPLICATE_BYTES};
use seek_clean::facts::{FileFacts, ScanFacts, Stat};
use seek_clean::protect::Places;
use seek_clean::remember::{self, Remembered};
use seek_core::DeviceState;
use seek_ipc::{Request, Response};

/// Counts the time spent in each kind of metadata call.
#[derive(Default)]
struct Timed {
    inner: ScanFacts,
    stat_us: AtomicU64,
    stat_n: AtomicU64,
    block_us: AtomicU64,
    block_n: AtomicU64,
}

fn timed<T>(us: &AtomicU64, n: &AtomicU64, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let out = f();
    us.fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
    n.fetch_add(1, Ordering::Relaxed);
    out
}

impl FileFacts for Timed {
    fn stat(&self, path: &Path) -> Option<Stat> {
        timed(&self.stat_us, &self.stat_n, || self.inner.stat(path))
    }
    fn same_bytes(&self, a: &Path, b: &Path) -> Result<bool, String> {
        self.inner.same_bytes(a, b)
    }
    fn file_id(&self, path: &Path) -> Option<(u32, u64)> {
        self.inner.file_id(path)
    }
    fn recycle_block(&self, path: &Path, batch_bytes: u64) -> Option<seek_clean::model::Lock> {
        timed(&self.block_us, &self.block_n, || self.inner.recycle_block(path, batch_bytes))
    }
}

fn secs(d: Duration) -> String {
    format!("{:.1}s", d.as_secs_f64())
}

pub fn run(fast: bool, fresh: bool) -> anyhow::Result<()> {
    let t0 = Instant::now();
    let batches: Vec<SizeBatch> = super::rt()?.block_on(async {
        let mut c = super::connect().await?;
        let Response::Devices { devices } = c.request(Request::ListDevices).await? else { bail!("unexpected reply") };
        let ids: Vec<_> = devices
            .iter()
            .filter(|d| d.state == DeviceState::Online && Path::new(&d.last_mount_point).exists())
            .map(|d| d.id)
            .collect();
        let mut out = Vec::new();
        let mut cursor = None;
        loop {
            let req = Request::SameSizeFiles { min_size: MIN_DUPLICATE_BYTES, device_ids: ids.clone(), cursor, limit: 2000 };
            let Response::SameSizeFiles { groups, next } = c.request(req).await? else { bail!("unexpected reply") };
            for g in groups {
                let files: Vec<SizeFile> = g
                    .files
                    .into_iter()
                    .filter(|r| !r.is_dir)
                    .map(|r| SizeFile { path: PathBuf::from(r.full_path), device_id: r.device_id.to_string() })
                    .collect();
                if files.len() >= 2 {
                    out.push(SizeBatch { size: g.size, files });
                }
            }
            match next {
                Some(n) => cursor = Some(n),
                None => break,
            }
        }
        Ok(out)
    })?;
    let files: usize = batches.iter().map(|b| b.files.len()).sum();
    let bytes: u64 = batches.iter().map(|b| b.size * b.files.len() as u64).sum();
    println!("paging   {}  {} sizes, {} files, {:.1} GB", secs(t0.elapsed()), batches.len(), files, bytes as f64 / 1e9);

    let t1 = Instant::now();
    let path = remember::default_path();
    let mut memory = if fresh { Remembered::default() } else { Remembered::load(&path) };
    println!("memory   {}", secs(t1.elapsed()));

    let places = Places::from_system().map_err(|e| anyhow::anyhow!("{e}"))?;
    let facts = Timed::default();
    let (fast, cancel) = (AtomicBool::new(fast), AtomicBool::new(false));
    let t2 = Instant::now();
    let mut first_read: Option<Duration> = None;
    let (mut groups, mut checked, mut last) = (0usize, 0u64, Instant::now());
    dupes::scan(batches, &places, &[], &facts, &[], &mut memory, &fast, &cancel, |e| match e {
        ScanEvent::Group(_) => groups += 1,
        ScanEvent::Progress { checked_bytes, total_bytes, .. } => {
            if checked_bytes > 0 && first_read.is_none() {
                first_read = Some(t2.elapsed());
            }
            checked = checked_bytes;
            if last.elapsed() >= Duration::from_secs(10) {
                last = Instant::now();
                let us = |a: &AtomicU64| Duration::from_micros(a.load(Ordering::Relaxed));
                println!(
                    "  {} read {:.1} / {:.1} GB  stat {}x {}  recycle {}x {}",
                    secs(t2.elapsed()),
                    checked_bytes as f64 / 1e9,
                    total_bytes as f64 / 1e9,
                    facts.stat_n.load(Ordering::Relaxed),
                    secs(us(&facts.stat_us)),
                    facts.block_n.load(Ordering::Relaxed),
                    secs(us(&facts.block_us))
                );
            }
        }
        ScanEvent::Done => {}
    });
    let us = |a: &AtomicU64| Duration::from_micros(a.load(Ordering::Relaxed));
    println!("scan     {}  first byte read after {}", secs(t2.elapsed()), first_read.map_or("-".into(), secs));
    println!("  stat         {} calls, {}", facts.stat_n.load(Ordering::Relaxed), secs(us(&facts.stat_us)));
    println!("  recycle      {} calls, {}", facts.block_n.load(Ordering::Relaxed), secs(us(&facts.block_us)));
    println!("  read {:.1} GB, {} groups", checked as f64 / 1e9, groups);
    if !fresh {
        memory.save(&path)?;
    }
    println!("total    {}", secs(t0.elapsed()));
    Ok(())
}
