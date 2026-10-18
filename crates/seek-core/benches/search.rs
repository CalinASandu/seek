//! Search benchmark on 10M synthetic entries (`cargo bench -p seek-core -- --quick`).
//!
//! Results (16-thread Windows dev box, shared with other builds, `--quick`, sorted by name):
//!   "report"              ~17.8 ms  (~209k hits)
//!   "*.pdf"               ~42 ms    (~366k hits)
//!   "ext:jpg size:>1mb"   ~31.6 ms  (~130k hits)
//!   ""                    ~490 ms   (10M hits, sort-dominated)
//! Target: "report" p95 < 50 ms on 10M entries.

use criterion::{criterion_group, criterion_main, Criterion};
use seek_core::query::parse;
use seek_core::*;

fn bench(c: &mut Criterion) {
    let index = synth_index(10_000_000, 1);
    let device = Device {
        id: uuid::Uuid::nil(),
        identity: DeviceIdentity::Volume {
            serial: 1,
            filesystem: "NTFS".into(),
        },
        display_name: "Bench".into(),
        label: String::new(),
        filesystem: "NTFS".into(),
        kind: DeviceKind::Fixed,
        total_bytes: 0,
        free_bytes: 0,
        last_mount_point: "C:\\".into(),
        indexed_by_host: uuid::Uuid::nil(),
        first_seen: 0,
        last_seen: 0,
        index_updated_at: 0,
        state: DeviceState::Online,
        entry_count: 0,
        error: None,
    };
    let devs = [SearchDevice {
        device: &device,
        index: &index,
        mount: "C:\\",
        online: true,
        pc: "",
    }];
    let mut group = c.benchmark_group("search_10M");
    group.sample_size(10);
    for q in ["report", "*.pdf", "ext:jpg size:>1mb", ""] {
        let expr = parse(q).unwrap();
        group.bench_function(format!("{q:?}"), |b| {
            b.iter(|| {
                let hits = search(&devs, &expr, SortKey::Name, false, &|| false).unwrap();
                std::hint::black_box(hits.len())
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
