//! Manual sanity check: `cargo run -p seek-scan --release --example walk -- <dir>`.
fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let t = std::time::Instant::now();
    let r = seek_scan::scan_walk(std::path::Path::new(&root), &|_| {}).unwrap();
    println!(
        "{} entries ({} skipped) in {:?}",
        r.index.live_count(),
        r.skipped,
        t.elapsed()
    );
}
