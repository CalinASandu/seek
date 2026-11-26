//! `== hub ==`: a PC's traffic with a Seek Hub, measured against a real in-process hub on loopback: uploading
//! and pulling a 2M-entry copy, a delta round trip, what an idle poll costs on the wire, and compacting a
//! journal (in a child process, so its peak memory is its own).

use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use seek_core::{decode_index, encode_index_level, synth_index, Change, IndexHeader, RawEntry, INDEX_FORMAT_VERSION};
use seek_hub::net::{Hub, SESSION};
use seek_hub::store::{PcRecord, Served};
use seek_hub::{HubConfig, HubKey, Store};
use seek_sync::noise::{self, Keys, SecureReader, SecureWriter};
use seek_sync::peers::key_to_b64;
use seek_sync::session::{decode_delta, encode_delta};
use seek_sync::{Cursor, Frame, Msg, Role, SYNC_PROTOCOL};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use uuid::Uuid;

use super::{device, mb, peak_working_set, Report, SYNC_DELTA, SYNC_ENTRIES};

/// Changes journaled on top of the full copy before it is compacted.
const COMPACT_CHANGES: usize = 10_000;
/// Idle polls averaged for the bytes-per-minute figure (the PC polls once a minute).
const IDLE_POLLS: u64 = 5;

/// Removes the directory when the section ends, however it ends.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A TCP stream that counts the bytes crossing it in both directions.
struct Counting {
    io: TcpStream,
    bytes: Arc<AtomicU64>,
}

impl AsyncRead for Counting {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.io).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &r {
            self.bytes.fetch_add((buf.filled().len() - before) as u64, Ordering::Relaxed);
        }
        r
    }
}

impl AsyncWrite for Counting {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        let r = Pin::new(&mut self.io).poll_write(cx, data);
        if let Poll::Ready(Ok(n)) = &r {
            self.bytes.fetch_add(*n as u64, Ordering::Relaxed);
        }
        r
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

/// A PC's authenticated session, opened as the real client does: kind byte, Noise handshake, hellos.
struct Pc {
    rd: SecureReader<ReadHalf<Counting>>,
    wr: SecureWriter<WriteHalf<Counting>>,
    wire: Arc<AtomicU64>,
}

impl Pc {
    async fn open(addr: std::net::SocketAddr, hub_public: &[u8; 32], host: Uuid, keys: &Keys) -> anyhow::Result<Pc> {
        let mut io = TcpStream::connect(addr).await?;
        io.set_nodelay(true)?;
        io.write_all(&[SESSION]).await?;
        let wire = Arc::new(AtomicU64::new(0));
        let (rd, wr) = noise::connect(Counting { io, bytes: wire.clone() }, keys, hub_public).await?;
        let mut pc = Pc { rd, wr, wire };
        let hello = Msg::Hello { protocol: SYNC_PROTOCOL, host_id: host, name: "perf".into(), role: Role::Pc };
        pc.send(&hello, vec![]).await?;
        anyhow::ensure!(matches!(pc.recv().await?.0, Msg::Hello { role: Role::Hub, .. }), "no hub hello");
        Ok(pc)
    }

    async fn send(&mut self, m: &Msg, payload: Vec<u8>) -> anyhow::Result<()> {
        Ok(self.wr.send(&Frame::new(m, payload)).await?)
    }

    async fn recv(&mut self) -> anyhow::Result<(Msg, Vec<u8>)> {
        let f = self.rd.recv().await?;
        Ok((f.msg()?, f.payload))
    }

    /// Polls like the client: `Pull` with the copies held; returns what the hub sent before `PullDone`.
    async fn pull(&mut self, cursors: Vec<(Uuid, Cursor)>) -> anyhow::Result<Vec<(Msg, Vec<u8>)>> {
        self.send(&Msg::Pull { cursors }, vec![]).await?;
        let mut got = Vec::new();
        loop {
            let (m, p) = self.recv().await?;
            if m == Msg::PullDone {
                return Ok(got);
            }
            got.push((m, p));
        }
    }
}

fn config(dir: &Path) -> HubConfig {
    HubConfig {
        name: "Perf Hub".into(),
        listen: vec!["127.0.0.1:0".parse().expect("loopback")],
        public: None,
        data_dir: dir.join("data"),
        keys_dir: dir.join("keys"),
        pc_quota_bytes: 2048 * 1024 * 1024,
        pc_max_devices: 64,
    }
}

/// Links a PC in the store directly (as pairing would).
fn link(store: &Store, n: u128) -> anyhow::Result<(Uuid, Keys)> {
    let keys = Keys::generate();
    let host = Uuid::from_u128(n);
    store.add_pc(PcRecord {
        host_id: host,
        name: format!("PC {n}"),
        public_key: key_to_b64(&keys.public),
        user_sid: format!("S-1-{n}"),
        user_name: format!("user{n}"),
        profiles: vec![],
        last_seen_ms: 0,
    })?;
    Ok((host, keys))
}

/// Upload, pull, delta round trip and idle poll against an in-process hub, then compaction in a child.
pub fn run(r: &mut Report, runs: usize) -> anyhow::Result<()> {
    r.section("hub");
    let dir = TempDir(std::env::temp_dir().join(format!("seek-perf-hub-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    let runs = runs.max(1);

    let ix = synth_index(SYNC_ENTRIES, 3);
    let header = IndexHeader {
        format_version: INDEX_FORMAT_VERSION,
        device_id: Uuid::new_v4(),
        host_id: Uuid::new_v4(),
        checkpoint: None,
        saved_at: 0,
        rows: 0,
        sync_cursor: Some((Uuid::new_v4(), 0)),
    };
    let bytes = encode_index_level(&header, &mut ix.clone(), 1)?;
    let note = format!("{SYNC_ENTRIES} entries, {:.1} MB", mb(bytes.len() as u64));
    let files: Vec<u64> = (0..ix.len() as u32).filter(|&row| !ix.is_dir(row)).map(|row| ix.id(row)).collect();
    anyhow::ensure!(files.len() >= SYNC_DELTA * runs, "too few files for {runs} delta runs");

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let cfg = config(&dir.0.join("live"));
    let key = HubKey::create(&cfg.keys_dir)?;
    let store = Arc::new(Store::open(&cfg, key)?);
    let hub = rt.block_on(Hub::start(cfg, store.clone()))?;
    let hub_public = store.noise_keys().public;
    let addr = hub.local_addrs()[0];
    let ((a_host, a_keys), (b_host, b_keys)) = (link(&store, 1)?, link(&store, 2)?);

    let mut dev = device("Perf drive", "D:\\");
    dev.indexed_by_host = a_host;
    let cursor = |seq| Cursor { generation: header.device_id, seq };

    let session = rt.block_on(async {
        let mut a = Pc::open(addr, &hub_public, a_host, &a_keys).await?;
        let mut b = Pc::open(addr, &hub_public, b_host, &b_keys).await?;
        a.send(&Msg::Devices { devices: vec![dev.clone()], profiles: vec![] }, vec![]).await?;
        anyhow::ensure!(matches!(a.recv().await?.0, Msg::Have { .. }), "no Have");

        // upload: the PC's first copy, to the acknowledgement
        let mut best = f64::MAX;
        for _ in 0..runs {
            let payload = bytes.clone();
            let t = Instant::now();
            a.send(&Msg::Full { device: dev.id, cursor: cursor(0) }, payload).await?;
            let (ack, _) = a.recv().await?;
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
            anyhow::ensure!(ack == Msg::Ack { device: dev.id, cursor: cursor(0) }, "upload refused: {ack:?}");
        }
        r.add("hub upload full 2M", best, "ms", &format!("send + encrypt + write + ack; {note}; target < 2000 ms"));

        // pull: another PC's first poll, to `PullDone`, then decoded
        let (mut best, mut best_decode) = (f64::MAX, f64::MAX);
        for _ in 0..runs {
            let t = Instant::now();
            let got = b.pull(vec![]).await?;
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
            let full = got.into_iter().find(|(m, _)| matches!(m, Msg::Full { .. }));
            let Some((Msg::Full { device: id, .. }, payload)) = full else { anyhow::bail!("the hub sent no full copy") };
            anyhow::ensure!(id == dev.id && payload.len() == bytes.len(), "wrong copy served");
            let t = Instant::now();
            std::hint::black_box(decode_index(&payload)?);
            best_decode = best_decode.min(t.elapsed().as_secs_f64() * 1000.0);
        }
        r.add("hub pull full 2M", best, "ms", &format!("read + decrypt + send to PullDone; {note}; target < 2000 ms"));
        r.add("hub pull full 2M decode", best_decode, "ms", "decoding the pulled copy; not in the figure above");

        // delta: one PC uploads 1,000 deletes, the other pulls and applies them
        let mut target = ix.clone();
        let (mut best, mut seq) = (f64::MAX, 0);
        for run in 0..runs {
            let ids = &files[run * SYNC_DELTA..(run + 1) * SYNC_DELTA];
            let before = target.live_count();
            let t = Instant::now();
            let changes: Vec<Change> = ids.iter().map(|&id| Change::Delete { id }).collect();
            let payload = encode_delta(&changes);
            a.send(&Msg::Delta { device: dev.id, from: cursor(seq), to: cursor(seq + 1) }, payload).await?;
            let (ack, _) = a.recv().await?;
            anyhow::ensure!(ack == Msg::Ack { device: dev.id, cursor: cursor(seq + 1) }, "delta refused: {ack:?}");
            let got = b.pull(vec![(dev.id, cursor(seq))]).await?;
            let Some((_, payload)) = got.into_iter().find(|(m, _)| matches!(m, Msg::Delta { .. })) else {
                anyhow::bail!("the hub sent no delta");
            };
            target.apply(&decode_delta(&payload)?);
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
            anyhow::ensure!(target.live_count() == before - SYNC_DELTA, "delta applied wrongly");
            seq += 1;
        }
        let note = "encode + upload + ack + pull + decode + apply; target < 50 ms";
        r.add("hub delta 1,000 round trip", best, "ms", note);

        // idle: nothing changed, so a poll is a `Pull` and the hub's catalog; the PC polls once a minute
        let before = b.wire.load(Ordering::Relaxed);
        for _ in 0..IDLE_POLLS {
            let got = b.pull(vec![(dev.id, cursor(seq))]).await?;
            anyhow::ensure!(got.iter().all(|(m, _)| matches!(m, Msg::Catalog { .. })), "an idle poll got data");
        }
        let per_poll = (b.wire.load(Ordering::Relaxed) - before) as f64 / IDLE_POLLS as f64;
        let note = format!("both directions incl. encryption, mean of {IDLE_POLLS} polls, 1 poll/min; 1 other PC, 1 device; target < 2048");
        r.add("hub idle poll bytes/min", per_poll, "B/min", &note);
        anyhow::Ok(())
    });
    hub.shutdown();
    drop(rt);
    session?;

    compaction(r, &dir.0, &bytes, &header, &files, ix.root_id())
}

/// Builds a store holding the full copy and `COMPACT_CHANGES` journaled changes, then compacts it in a child
/// `seek hub-compact` so the child's peak memory is the compaction's own.
fn compaction(r: &mut Report, dir: &Path, bytes: &[u8], header: &IndexHeader, files: &[u64], root: u64) -> anyhow::Result<()> {
    let cfg = config(&dir.join("compact"));
    let key = HubKey::create(&cfg.keys_dir)?;
    let store = Store::open(&cfg, key)?;
    let (host, _) = link(&store, 1)?;
    let mut dev = device("Perf drive", "D:\\");
    dev.indexed_by_host = host;
    let cursor = |seq| Cursor { generation: header.device_id, seq };
    let refused = |e| anyhow::anyhow!("hub refused: {e:?}");
    store.set_pc_devices(host, vec![dev.clone()], vec![]).map_err(refused)?;
    store.put_full(host, dev.id, cursor(0), bytes.to_vec()).map_err(refused)?;
    // 1,000 changes a batch: deletes of existing files alternate with new files under the root
    let batch = 1_000;
    for k in 0..COMPACT_CHANGES / batch {
        let changes: Vec<Change> = (k * batch..(k + 1) * batch)
            .map(|n| match n % 2 {
                0 => Change::Delete { id: files[n] },
                _ => Change::Upsert(RawEntry {
                    id: u64::MAX / 2 + n as u64,
                    parent_id: root,
                    name: format!("added {n}.txt"),
                    size: 1234,
                    modified: 1_700_000_000_000,
                    created: 1_700_000_000_000,
                    attrs: 0,
                }),
            })
            .collect();
        let (from, to) = (cursor(k as u64), cursor(k as u64 + 1));
        store.put_delta(host, dev.id, from, to, encode_delta(&changes)).map_err(|e| anyhow::anyhow!("hub refused: {e:?}"))?;
    }
    drop(store);

    let mut child = Command::new(std::env::current_exe()?)
        .arg("hub-compact")
        .arg(dir)
        .arg(dev.id.to_string())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let mut out = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut().expect("piped"), &mut out)?;
    let status = child.wait()?;
    let peak = peak_working_set(Some(child.as_raw_handle() as isize));
    anyhow::ensure!(status.success(), "hub-compact failed");
    let ms: f64 = out.lines().last().unwrap_or("").trim().parse()?;
    let note = format!("{SYNC_ENTRIES} entries + {COMPACT_CHANGES} changes folded into the full copy");
    r.add("hub compaction 2M + 10k changes", ms, "ms", &note);
    r.add("hub compaction peak working set", mb(peak), "MB", "whole child process, store open included");
    Ok(())
}

/// This hidden command rewrites a store: it only touches a directory `seek perf` made, i.e. one named
/// `seek-perf-hub-*` directly inside the temp directory.
fn check_perf_dir(dir: &Path) -> anyhow::Result<()> {
    let dir = dir.canonicalize()?;
    let tmp = std::env::temp_dir().canonicalize()?;
    let ok = dir.parent() == Some(tmp.as_path())
        && dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with("seek-perf-hub-"));
    anyhow::ensure!(ok, "hub-compact only works on a seek-perf-hub-* directory in the temp directory");
    Ok(())
}

/// The child of `compaction` (`seek hub-compact <dir> <device>`): opens the store in `<dir>/compact`, compacts
/// `device` and prints the milliseconds that took.
pub fn compact_child(dir: &Path, device: Uuid) -> anyhow::Result<()> {
    check_perf_dir(dir)?;
    let cfg = config(&dir.join("compact"));
    let _lock = seek_hub::lock::DataLock::try_take(&cfg.data_dir)?;
    let store = Store::open(&cfg, HubKey::load(&cfg.keys_dir)?)?;
    let t = Instant::now();
    store.compact(device)?;
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    anyhow::ensure!(matches!(store.serve(device, None).as_slice(), [Served::Full { .. }]), "the journal was not folded");
    println!("{ms}");
    Ok(())
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    #[test]
    fn only_perf_dirs_in_temp_are_accepted() {
        let tmp = std::env::temp_dir();
        let ok = tmp.join(format!("seek-perf-hub-guard-{}", std::process::id()));
        std::fs::create_dir_all(&ok).unwrap();
        assert!(check_perf_dir(&ok).is_ok());
        let wrong_name = tmp.join(format!("seek-guard-other-{}", std::process::id()));
        std::fs::create_dir_all(&wrong_name).unwrap();
        assert!(check_perf_dir(&wrong_name).is_err());
        let nested = ok.join("seek-perf-hub-inner");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(check_perf_dir(&nested).is_err());
        assert!(check_perf_dir(Path::new(".")).is_err());
        let _ = std::fs::remove_dir_all(&ok);
        let _ = std::fs::remove_dir_all(&wrong_name);
    }
}
