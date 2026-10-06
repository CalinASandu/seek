//! `seek perf`: one command that measures every performance metric Seek cares about, prints them,
//! and optionally saves them as JSON and compares them with an earlier run.
//!
//! Sections: synthetic index (build time, memory), search latency for every query × sort key,
//! fetching a page of results, saving/loading an index, applying a batch of live changes, real
//! drive scans (each in a child process so its peak memory is its own), and round trips to the
//! running service.

use std::collections::HashMap;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::Context;
use seek_core::{
    load_index, materialize, query, save_index, search, synth_index, Change, Device, DeviceIdentity,
    DeviceIndex, DeviceKind, DeviceState, IndexHeader, RawEntry, SearchDevice, SortKey,
    ATTR_DIRECTORY, INDEX_FORMAT_VERSION,
};
use seek_ipc::{Client, Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

pub struct Opts {
    pub entries: usize,
    pub runs: usize,
    pub scans: Vec<String>,
    pub remote: bool,
    pub json: Option<PathBuf>,
    pub compare: Option<PathBuf>,
}

const QUERIES: &[&str] = &["report", "*.pdf", "ext:jpg size:>1mb", "dm:2024 path:photo", "regex:^report_\\d+", ""];
const SORTS: &[(SortKey, &str)] = &[
    (SortKey::Name, "name"),
    (SortKey::Path, "path"),
    (SortKey::Size, "size"),
    (SortKey::Modified, "modified"),
    (SortKey::Device, "device"),
];
/// Path sorting builds every hit's full path; above this many hits it is measured once, not `runs` times.
const PATH_SORT_SINGLE_RUN: usize = 1_000_000;
const PAGE: usize = 500;

struct Report {
    metrics: Vec<(String, f64, &'static str)>,
    baseline: HashMap<String, f64>,
}

impl Report {
    fn section(&self, title: &str) {
        println!("\n== {title} ==");
    }

    fn add(&mut self, name: impl Into<String>, value: f64, unit: &'static str, note: &str) {
        let name = name.into();
        let delta = match self.baseline.get(&name) {
            Some(&old) if old > 0.0 && unit != "hits" && unit != "entries" => {
                let pct = (value - old) / old * 100.0;
                let better = if unit.ends_with("/s") { pct > 0.0 } else { pct < 0.0 };
                let tag = if pct.abs() < 5.0 { "  ~" } else if better { "  better" } else { "  WORSE" };
                format!("{pct:+7.1}% (was {old:.1}){tag}")
            }
            _ => String::new(),
        };
        println!("{name:<46} {value:>12.1} {unit:<9} {note}{delta}");
        self.metrics.push((name, value, unit));
    }
}

fn device(name: &str, mount: &str) -> Device {
    Device {
        id: uuid::Uuid::new_v4(),
        identity: DeviceIdentity::Volume { serial: 0, filesystem: "NTFS".into() },
        display_name: name.into(),
        label: String::new(),
        filesystem: "NTFS".into(),
        kind: DeviceKind::Fixed,
        total_bytes: 0,
        free_bytes: 0,
        last_mount_point: mount.into(),
        indexed_by_host: uuid::Uuid::nil(),
        first_seen: 0,
        last_seen: 0,
        index_updated_at: 0,
        state: DeviceState::Online,
        entry_count: 0,
        error: None,
    }
}

/// Best and median wall time of `runs` calls, in ms.
fn time_ms(runs: usize, mut f: impl FnMut()) -> (f64, f64) {
    let mut t: Vec<f64> = (0..runs.max(1))
        .map(|_| {
            let s = Instant::now();
            f();
            s.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    t.sort_by(f64::total_cmp);
    (t[0], t[t.len() / 2])
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Peak working set of a process (this one when `handle` is `None`).
fn peak_working_set(handle: Option<isize>) -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain C struct filled by the API; the handle is a valid process handle.
    unsafe {
        let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let h = handle.map(|h| h as _).unwrap_or_else(|| GetCurrentProcess());
        if GetProcessMemoryInfo(h, &mut c, c.cb) == 0 {
            return 0;
        }
        c.PeakWorkingSetSize as u64
    }
}

fn load_baseline(path: &Path) -> anyhow::Result<HashMap<String, f64>> {
    let v: Value = serde_json::from_slice(&std::fs::read(path).with_context(|| format!("reading {}", path.display()))?)?;
    Ok(v["metrics"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| Some((m["name"].as_str()?.to_string(), m["value"].as_f64()?)))
                .collect()
        })
        .unwrap_or_default())
}

pub fn run(o: Opts) -> anyhow::Result<()> {
    let baseline = match &o.compare {
        Some(p) => load_baseline(p)?,
        None => HashMap::new(),
    };
    let mut r = Report { metrics: Vec::new(), baseline };
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!(
        "seek perf — {} synthetic entries on 2 devices, best of {} runs, {threads} threads{}",
        o.entries,
        o.runs,
        o.compare.as_ref().map(|p| format!(", comparing with {}", p.display())).unwrap_or_default()
    );

    synthetic(&mut r, &o)?;
    for target in &o.scans {
        real_scan(&mut r, target)?;
    }
    if o.remote {
        remote(&mut r, o.runs)?;
    }

    if let Some(path) = &o.json {
        let metrics: Vec<Value> =
            r.metrics.iter().map(|(n, v, u)| json!({ "name": n, "value": v, "unit": u })).collect();
        let doc = json!({
            "version": 1,
            "savedAt": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64,
            "entries": o.entries,
            "runs": o.runs,
            "threads": threads,
            "metrics": metrics,
        });
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(&doc)?)?;
        println!("\nsaved {}", path.display());
    }
    Ok(())
}

fn synthetic(r: &mut Report, o: &Opts) -> anyhow::Result<()> {
    r.section("synthetic index");
    let half = o.entries / 2;
    let t = Instant::now();
    let (mut a, b) = std::thread::scope(|s| {
        let b = s.spawn(|| synth_index(o.entries - half, 2));
        (synth_index(half, 1), b.join().expect("synth thread"))
    });
    r.add("index/build", t.elapsed().as_secs_f64() * 1000.0, "ms", "");
    let heap = (a.heap_bytes() + b.heap_bytes()) as u64;
    r.add("index/memory", mb(heap), "MB", "");
    r.add("index/bytes_per_entry", heap as f64 / o.entries.max(1) as f64, "B", "");

    let (da, db) = (device("Synthetic A", "S:\\"), device("Synthetic B", "T:\\"));
    {
        let devs = [
            SearchDevice { device: &da, index: &a, mount: "S:\\", online: true },
            SearchDevice { device: &db, index: &b, mount: "T:\\", online: false },
        ];

        r.section("search (query × sort)");
        for q in QUERIES {
            let expr = query::parse(q).map_err(|e| anyhow::anyhow!("{q}: {}", e.message))?;
            let hits = search(&devs, &expr, SortKey::Name, false, &|| false).map(|h| h.len()).unwrap_or(0);
            for &(sort, sname) in SORTS {
                let runs = if sort == SortKey::Path && hits > PATH_SORT_SINGLE_RUN { 1 } else { o.runs };
                let (best, median) = time_ms(runs, || {
                    std::hint::black_box(search(&devs, &expr, sort, false, &|| false));
                });
                let note = format!("{hits} hits, median {median:.1}");
                r.add(format!("search/{q:?}/{sname}"), best, "ms", &note);
            }
        }
        let expr = query::parse("report").expect("valid");
        let (best, _) = time_ms(o.runs, || {
            std::hint::black_box(search(&devs, &expr, SortKey::Size, true, &|| false));
        });
        r.add("search/\"report\"/size desc", best, "ms", "");

        r.section("results page");
        let hits = search(&devs, &expr, SortKey::Name, false, &|| false).unwrap_or_default();
        let page: Vec<_> = hits.iter().take(PAGE).copied().collect();
        let (best, _) = time_ms(o.runs, || {
            for h in &page {
                std::hint::black_box(materialize(&devs, *h));
            }
        });
        r.add(format!("fetch/{PAGE} rows"), best, "ms", "");
    }

    r.section("persistence");
    let path = std::env::temp_dir().join(format!("seek-perf-{}.seekidx", std::process::id()));
    let header = IndexHeader {
        format_version: INDEX_FORMAT_VERSION,
        device_id: da.id,
        host_id: uuid::Uuid::nil(),
        checkpoint: None,
        saved_at: 0,
        rows: 0,
    };
    let t = Instant::now();
    save_index(&path, &header, &mut a)?;
    r.add("index/save", t.elapsed().as_secs_f64() * 1000.0, "ms", &format!("{half} entries"));
    r.add("index/file size", mb(std::fs::metadata(&path)?.len()), "MB", "");
    let t = Instant::now();
    let (_, loaded) = load_index(&path)?;
    r.add("index/load", t.elapsed().as_secs_f64() * 1000.0, "ms", "");
    drop(loaded);
    let _ = std::fs::remove_file(&path);

    r.section("live changes");
    let (n, ms) = apply_batch(&mut a);
    r.add("apply/batch", ms, "ms", &format!("{n} changes incl. folder deletes"));

    r.section("process");
    r.add("process/peak working set", mb(peak_working_set(None)), "MB", "synthetic section");
    Ok(())
}

/// A journal-like batch: new files under existing folders, renames, file deletes and folder deletes.
fn apply_batch(ix: &mut DeviceIndex) -> (usize, f64) {
    let rows = ix.len() as u32;
    let dirs: Vec<u32> = (0..rows).filter(|&r| ix.is_alive(r) && ix.is_dir(r)).take(2000).collect();
    let files: Vec<u32> = (0..rows).filter(|&r| ix.is_alive(r) && !ix.is_dir(r)).take(2000).collect();
    let entry = |id, parent_id, name: String, attrs| RawEntry {
        id,
        parent_id,
        name,
        size: 1234,
        modified: 1_700_000_000_000,
        created: 1_700_000_000_000,
        attrs,
    };
    let mut changes = Vec::new();
    for (i, d) in dirs.iter().take(1000).enumerate() {
        changes.push(Change::Upsert(entry(u64::MAX / 2 + i as u64, ix.id(*d), format!("new file {i}.txt"), 0)));
    }
    for (i, f) in files.iter().take(500).enumerate() {
        let parent = ix.parent(*f).map(|p| ix.id(p)).unwrap_or(ix.root_id());
        changes.push(Change::Upsert(entry(ix.id(*f), parent, format!("renamed {i}.txt"), 0)));
    }
    for f in files.iter().skip(500).take(500) {
        changes.push(Change::Delete { id: ix.id(*f) });
    }
    for d in dirs.iter().skip(1000).take(5) {
        changes.push(Change::Delete { id: ix.id(*d) });
    }
    // a new folder whose files arrive before it (journal order after de-duplication)
    changes.push(Change::Upsert(entry(u64::MAX / 2 + 5000, u64::MAX / 2 + 5001, "inner.txt".into(), 0)));
    changes.push(Change::Upsert(entry(u64::MAX / 2 + 5001, ix.root_id(), "fresh folder".into(), ATTR_DIRECTORY)));
    let t = Instant::now();
    ix.apply(&changes);
    (changes.len(), t.elapsed().as_secs_f64() * 1000.0)
}

/// Scans `target` in a child `seek scan --json` so the child's peak memory is the scan's own.
fn real_scan(r: &mut Report, target: &str) -> anyhow::Result<()> {
    r.section(&format!("scan {target}"));
    let exe = std::env::current_exe()?;
    let mut child = Command::new(exe)
        .args(["scan", target, "--json"])
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let mut out = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut().expect("piped"), &mut out)?;
    let status = child.wait()?;
    let peak = peak_working_set(Some(child.as_raw_handle() as isize));
    anyhow::ensure!(status.success(), "scan of {target} failed");
    let v: Value = serde_json::from_str(out.lines().last().unwrap_or("{}"))?;
    let entries = v["entries"].as_f64().unwrap_or(0.0);
    let secs = v["seconds"].as_f64().unwrap_or(0.0);
    let method = v["method"].as_str().unwrap_or("?").to_string();
    let key = target.trim_end_matches('\\').to_string();
    r.add(format!("scan/{key}/entries"), entries, "entries", &method);
    r.add(format!("scan/{key}/time"), secs, "s", &method);
    r.add(format!("scan/{key}/rate"), entries / secs.max(1e-9) / 1000.0, "k/s", "");
    r.add(format!("scan/{key}/index memory"), mb(v["indexBytes"].as_u64().unwrap_or(0)), "MB", "");
    r.add(format!("scan/{key}/peak working set"), mb(peak), "MB", "whole scan process");
    Ok(())
}

/// Round trips through the running service: Search, then the first page of rows.
fn remote(r: &mut Report, runs: usize) -> anyhow::Result<()> {
    r.section("service round trips");
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let mut c = match Client::connect().await {
            Ok(c) => c,
            Err(e) => {
                println!("service not reachable ({e}); skipped");
                return Ok(());
            }
        };
        c.request(Request::Hello { protocol_version: PROTOCOL_VERSION }).await?;
        for q in ["report", "ext:pdf", ""] {
            let mut times = Vec::new();
            let (mut total, mut server_ms) = (0, f64::MAX);
            for _ in 0..runs.max(1) {
                let t = Instant::now();
                let Response::SearchStarted { search_id, total: n, elapsed_ms } =
                    c.request(Request::Search { query: q.into(), sort: SortKey::Name, descending: false }).await?
                else {
                    anyhow::bail!("search {q:?} failed");
                };
                c.request(Request::FetchRows { search_id, offset: 0, limit: 200 }).await?;
                times.push(t.elapsed().as_secs_f64() * 1000.0);
                total = n;
                server_ms = server_ms.min(elapsed_ms);
            }
            times.sort_by(f64::total_cmp);
            let note = format!("{total} hits, service {server_ms:.1} ms, median {:.1}", times[times.len() / 2]);
            r.add(format!("remote/{q:?}/search+page"), times[0], "ms", &note);
        }
        anyhow::Ok(())
    })
}
