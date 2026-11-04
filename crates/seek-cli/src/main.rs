//! `seek` — developer CLI: list volumes, scan, query an index file, query the running service, perf.

mod perf;

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use seek_core::{
    load_index, materialize, query, save_index, search, Device, DeviceIdentity,
    DeviceKind, DeviceState, IndexHeader, ResultRow, SearchDevice, SortKey,
    INDEX_FORMAT_VERSION,
};
use seek_ipc::{Client, Request, Response, PROTOCOL_VERSION};

#[derive(Parser)]
#[command(name = "seek", version, about = "Seek developer CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List local volumes.
    Volumes,
    /// Scan a drive (e.g. `C:\`) or folder and optionally save the index.
    Scan {
        target: String,
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print one JSON line with the measurements (used by `seek perf`).
        #[arg(long)]
        json: bool,
    },
    /// Search a saved .seekidx file.
    Query {
        file: PathBuf,
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Search through the running seek-service.
    Remote {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long, value_enum, default_value = "name")]
        sort: SortArg,
        #[arg(long)]
        desc: bool,
    },
    /// Show the service's devices.
    Devices,
    /// Show what the service's memory is made of, and which code last used each index.
    Memory,
    /// Show the hub link: when this PC and the others last synced, and each drive's dates.
    Hub,
    /// Measure every performance metric: synthetic index, searches × sorts, persistence, live
    /// changes, optional real scans and service round trips. `--json` saves, `--compare` diffs.
    Perf {
        /// Synthetic entries (split over two devices).
        #[arg(long, default_value_t = 10_000_000)]
        entries: usize,
        /// Timed runs per measurement (the best is reported).
        #[arg(long, default_value_t = 5)]
        runs: usize,
        /// Also scan these drives/folders (repeatable), e.g. `--scan D:\`.
        #[arg(long = "scan")]
        scans: Vec<String>,
        /// Also time searches through the running seek-service.
        #[arg(long)]
        remote: bool,
        /// Save the results as JSON.
        #[arg(long)]
        json: Option<PathBuf>,
        /// Compare with results saved earlier by `--json`.
        #[arg(long)]
        compare: Option<PathBuf>,
    },
    /// Used by `seek perf`: compact one device of a hub store in this process.
    #[command(hide = true)]
    HubCompact { dir: PathBuf, device: uuid::Uuid },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum SortArg {
    Name,
    Path,
    Size,
    Modified,
    Device,
}

impl From<SortArg> for SortKey {
    fn from(s: SortArg) -> Self {
        match s {
            SortArg::Name => SortKey::Name,
            SortArg::Path => SortKey::Path,
            SortArg::Size => SortKey::Size,
            SortArg::Modified => SortKey::Modified,
            SortArg::Device => SortKey::Device,
        }
    }
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Volumes => volumes(),
        Cmd::Scan { target, out, json } => scan(&target, out.as_deref(), json),
        Cmd::Query { file, query, limit } => query_file(&file, &query, limit),
        Cmd::Remote { query, limit, sort, desc } => remote(&query, limit, sort.into(), desc),
        Cmd::Devices => devices(),
        Cmd::Memory => memory(),
        Cmd::Hub => hub(),
        Cmd::HubCompact { dir, device } => perf::hub_compact(&dir, device),
        Cmd::Perf { entries, runs, scans, remote, json, compare } => {
            perf::run(perf::Opts { entries, runs, scans, remote, json, compare })
        }
    }
}

fn volumes() -> anyhow::Result<()> {
    println!("{:<6} {:<8} {:<10} {:<20} {:>12} {:>12}", "MOUNT", "FS", "KIND", "LABEL", "TOTAL", "FREE");
    for v in seek_scan::list_volumes() {
        println!(
            "{:<6} {:<8} {:<10} {:<20} {:>12} {:>12}",
            v.mount_point,
            v.filesystem,
            format!("{:?}", v.kind),
            v.label,
            human(v.total_bytes),
            human(v.free_bytes)
        );
    }
    Ok(())
}

fn scan(target: &str, out: Option<&Path>, json: bool) -> anyhow::Result<()> {
    let started = Instant::now();
    let progress = |_n: u64| {};
    let is_drive = target.len() <= 3 && target.chars().nth(1) == Some(':');
    let result = if is_drive {
        let mount = format!("{}:\\", &target[..1].to_uppercase());
        let vol = seek_scan::list_volumes()
            .into_iter()
            .find(|v| v.mount_point.eq_ignore_ascii_case(&mount))
            .with_context(|| format!("no volume at {mount}"))?;
        let r = seek_scan::scan_volume(&vol, &progress)?;
        if vol.filesystem.eq_ignore_ascii_case("NTFS") && r.index.root_id() != seek_scan::ntfs::NTFS_ROOT_ID && !json {
            eprintln!("MFT scan unavailable; walked directories instead (run elevated for the fast path)");
        }
        r
    } else {
        seek_scan::scan_walk(Path::new(target), &progress)?
    };
    let method = if result.index.root_id() == seek_scan::ntfs::NTFS_ROOT_ID { "NTFS MFT" } else { "directory walk" };
    let scanned = started.elapsed();
    let skipped = result.skipped;
    let journal = result.checkpoint.is_some();
    let mut ix = result.index;
    ix.checkpoint = result.checkpoint;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "entries": ix.live_count(),
                "seconds": scanned.as_secs_f64(),
                "method": method,
                "skipped": skipped,
                "journal": journal,
                "indexBytes": ix.heap_bytes(),
            })
        );
    } else {
        println!(
            "{} entries via {method} in {:.2}s ({skipped} skipped, USN checkpoint: {journal})",
            ix.live_count(),
            scanned.as_secs_f64()
        );
    }
    if let Some(out) = out {
        let header = IndexHeader {
            format_version: INDEX_FORMAT_VERSION,
            device_id: uuid::Uuid::new_v4(),
            host_id: uuid::Uuid::nil(),
            checkpoint: ix.checkpoint,
            saved_at: chrono_now(),
            rows: 0,
            sync_cursor: None,
        };
        // The mount point is needed later to print paths; keep it next to the index.
        save_index(out, &header, &mut ix)?;
        std::fs::write(out.with_extension("mount"), mount_of(target))?;
        println!("saved {} ({})", out.display(), human(std::fs::metadata(out)?.len()));
    }
    Ok(())
}

fn mount_of(target: &str) -> String {
    let t = target.trim_end_matches('\\');
    if t.len() == 2 && t.ends_with(':') {
        format!("{t}\\")
    } else {
        t.to_string()
    }
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn query_file(file: &Path, q: &str, limit: usize) -> anyhow::Result<()> {
    let t = Instant::now();
    let (hdr, ix) = load_index(file)?;
    println!("loaded {} entries in {:.0} ms", ix.live_count(), t.elapsed().as_secs_f64() * 1000.0);
    let mount = std::fs::read_to_string(file.with_extension("mount")).unwrap_or_else(|_| "?:\\".into());
    let device = Device {
        id: hdr.device_id,
        identity: DeviceIdentity::Volume { serial: 0, filesystem: String::new() },
        display_name: file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        label: String::new(),
        filesystem: String::new(),
        kind: DeviceKind::Fixed,
        total_bytes: 0,
        free_bytes: 0,
        last_mount_point: mount.clone(),
        indexed_by_host: hdr.host_id,
        first_seen: 0,
        last_seen: 0,
        index_updated_at: hdr.saved_at,
        state: DeviceState::Online,
        entry_count: ix.live_count() as u64,
        error: None,
    };
    let expr = query::parse(q).map_err(|e| anyhow::anyhow!("{} (at {})", e.message, e.position))?;
    let devs = [SearchDevice { device: &device, index: &ix, mount: &mount, online: true, pc: "" }];
    let t = Instant::now();
    let hits = search(&devs, &expr, SortKey::Name, false, &|| false).unwrap_or_default();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let rows: Vec<ResultRow> = hits.iter().take(limit).map(|h| materialize(&devs, *h)).collect();
    print_rows(&rows);
    println!("{} results in {ms:.1} ms", hits.len());
    Ok(())
}

fn rt() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?)
}

async fn connect() -> anyhow::Result<Client> {
    let mut c = Client::connect()
        .await
        .context("cannot reach seek-service — start it with `cargo run -p seek-service -- --console`")?;
    match c.request(Request::Hello { protocol_version: PROTOCOL_VERSION }).await? {
        Response::Hello { .. } => Ok(c),
        other => bail!("unexpected hello reply: {other:?}"),
    }
}

fn remote(q: &str, limit: u32, sort: SortKey, descending: bool) -> anyhow::Result<()> {
    rt()?.block_on(async {
        let mut c = connect().await?;
        let (id, total, ms) = match c.request(Request::Search { query: q.into(), sort, descending }).await? {
            Response::SearchStarted { search_id, total, elapsed_ms } => (search_id, total, elapsed_ms),
            Response::SearchError { message, position } => bail!("query error at {position}: {message}"),
            other => bail!("unexpected reply: {other:?}"),
        };
        let rows = match c.request(Request::FetchRows { search_id: id, offset: 0, limit }).await? {
            Response::Rows { rows, .. } => rows,
            other => bail!("unexpected reply: {other:?}"),
        };
        print_rows(&rows);
        println!("{total} results in {ms:.1} ms");
        Ok(())
    })
}

/// `90 s ago`, `12 min ago`, `3 h ago`, `2 days ago`; `never` for 0.
fn ago(ms: i64) -> String {
    if ms <= 0 {
        return "never".into();
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    match (now - ms) / 1000 {
        s if s < 120 => format!("{s} s ago"),
        s if s < 7200 => format!("{} min ago", s / 60),
        s if s < 172_800 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86_400),
    }
}

fn hub() -> anyhow::Result<()> {
    rt()?.block_on(async {
        let mut c = connect().await?;
        let Response::Hub { hub } = c.request(Request::HubStatus).await? else { bail!("unexpected reply") };
        match hub {
            None => println!("Not linked to a hub."),
            Some(h) => {
                println!("Hub {} at {}: {}, synced {}", h.name, h.addr, if h.online { "connected" } else { "not connected" }, ago(h.last_sync_ms));
                if let Some(e) = h.last_error {
                    println!("  last error: {e}");
                }
                for pc in h.pcs {
                    println!("  PC {}: {}, hub last saw it {}", pc.name, if pc.online { "online" } else { "offline" }, ago(pc.last_sync_ms));
                }
            }
        }
        let Response::Devices { devices } = c.request(Request::ListDevices).await? else { bail!("unexpected reply") };
        println!();
        println!("{:<24} {:<9} {:>10}  {:<14}  LAST SEEN", "DRIVE", "STATE", "FILES", "INDEX UPDATED");
        for d in devices {
            println!(
                "{:<24} {:<9} {:>10}  {:<14}  {}",
                d.display_name.chars().take(24).collect::<String>(),
                format!("{:?}", d.state),
                d.entry_count,
                ago(d.index_updated_at),
                ago(d.last_seen)
            );
        }
        Ok(())
    })
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

fn memory() -> anyhow::Result<()> {
    rt()?.block_on(async {
        let mut c = connect().await?;
        let Response::Memory { report: r } = c.request(Request::Memory).await? else {
            bail!("unexpected reply (is the service older than this command?)")
        };
        println!("Task Manager (working set)  {:>10}    peak {}", mb(r.working_set), mb(r.peak_working_set));
        println!("Committed (private)         {:>10}", mb(r.private_bytes));
        println!("Heap in use                 {:>10}    peak {}", mb(r.heap_live), mb(r.heap_peak));
        match r.idle_unload_secs {
            Some(s) => println!("Idle unloading              after {s} s, checked {} times", r.idle_checks),
            None => println!("Idle unloading              off"),
        }
        println!();
        println!("{:<24} {:<10} {:>10} {:>10} {:>7} {:>5} {:>7}  LAST USED BY", "DRIVE", "WHERE", "ROWS", "RAM", "IDLE", "LOADS", "UNLOADS");
        for d in &r.devices {
            let place = if d.in_memory { "memory" } else if d.on_disk_only { "disk" } else { "no index" };
            let idle = d.idle_secs.map_or("-".to_string(), |s| format!("{s} s"));
            println!(
                "{:<24} {:<10} {:>10} {:>10} {:>7} {:>5} {:>7}  {}",
                d.name.chars().take(24).collect::<String>(),
                place,
                d.rows,
                mb(d.bytes),
                idle,
                d.loads,
                d.unloads,
                d.last_used_by.as_deref().unwrap_or("-")
            );
            if let Some(why) = &d.refused {
                println!("    stays in memory: {why}");
            }
        }
        let indexes: u64 = r.devices.iter().map(|d| d.bytes).sum();
        let known = indexes + r.journal_bytes + r.mask_bytes + r.totals_bytes + r.stored_result_bytes;
        println!();
        println!("Heap by owner");
        for (name, bytes) in [
            ("indexes", indexes),
            ("change journals", r.journal_bytes),
            ("hidden-folder masks", r.mask_bytes),
            ("Space totals", r.totals_bytes),
            ("stored search results", r.stored_result_bytes),
            ("everything else", r.heap_live.saturating_sub(known)),
        ] {
            println!("  {name:<24} {:>10}", mb(bytes));
        }
        println!("  {:<24} {:>10}", "committed beyond the heap", mb(r.private_bytes.saturating_sub(r.heap_live)));
        let mut columns: std::collections::BTreeMap<&str, u64> = Default::default();
        for d in &r.devices {
            for (name, bytes) in &d.columns {
                *columns.entry(name).or_default() += bytes;
            }
        }
        if indexes > 0 {
            let mut columns: Vec<_> = columns.into_iter().collect();
            columns.sort_by_key(|c| std::cmp::Reverse(c.1));
            let rows: u64 = r.devices.iter().map(|d| d.rows).sum::<u64>().max(1);
            println!();
            println!("Indexes by column ({rows} rows)");
            for (name, bytes) in columns {
                println!("  {name:<24} {:>10}  {:>5.1} B/row", mb(bytes), bytes as f64 / rows as f64);
            }
        }
        println!();
        println!("Who asked for indexes since the service started");
        for (who, n) in &r.touches {
            println!("  {who:<40} {n:>8}");
        }
        Ok(())
    })
}

fn devices() -> anyhow::Result<()> {
    rt()?.block_on(async {
        let mut c = connect().await?;
        let Response::Devices { devices } = c.request(Request::ListDevices).await? else {
            bail!("unexpected reply")
        };
        println!("{:<28} {:<9} {:<8} {:>12}  ID", "NAME", "STATE", "FS", "ENTRIES");
        for d in devices {
            println!(
                "{:<28} {:<9} {:<8} {:>12}  {}",
                d.display_name,
                format!("{:?}", d.state),
                d.filesystem,
                d.entry_count,
                d.id
            );
        }
        Ok(())
    })
}

fn print_rows(rows: &[ResultRow]) {
    for r in rows {
        let size = if r.is_dir { "<dir>".to_string() } else { human(r.size) };
        let state = if r.online { "" } else { "  [offline]" };
        println!("{:>10}  {}{state}", size, r.full_path);
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}
