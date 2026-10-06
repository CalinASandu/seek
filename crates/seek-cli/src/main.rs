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
    let devs = [SearchDevice { device: &device, index: &ix, mount: &mount, online: true }];
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
