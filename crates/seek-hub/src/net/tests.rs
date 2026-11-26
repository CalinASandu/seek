use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use seek_sync::noise::{self, Keys, SecureReader, SecureWriter};
use seek_sync::pairing::{pair_guest, PairInfo, HUB_CODE_DIGITS};
use seek_sync::peers::key_to_b64;
use seek_sync::{Frame, Msg, Role, SYNC_PROTOCOL};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::time::Instant;
use uuid::Uuid;

use super::*;
use crate::crypt::HubKey;
use crate::store::PcRecord;

struct Env {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    hub: Arc<Hub>,
    addr: SocketAddr,
}

impl Env {
    fn hub_public(&self) -> [u8; 32] {
        self.store.noise_keys().public
    }
}

async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let cfg = HubConfig {
        name: "Test Hub".into(),
        listen: vec!["127.0.0.1:0".parse().unwrap()],
        public: None,
        data_dir: dir.path().join("data"),
        keys_dir: dir.path().join("keys"),
        pc_quota_bytes: 2048 * 1024 * 1024,
        pc_max_devices: 64,
    };
    let key = HubKey::create(&cfg.keys_dir).unwrap();
    let store = Arc::new(Store::open(&cfg, key).unwrap());
    let hub = Hub::start(cfg, store.clone()).await.unwrap();
    let addr = hub.local_addrs()[0];
    Env { _dir: dir, store, hub, addr }
}

/// Links PC `n` in the store directly (as pairing would).
fn linked(store: &Store, n: u128) -> (PcRecord, Keys) {
    let keys = Keys::generate();
    let rec = PcRecord {
        host_id: Uuid::from_u128(n),
        name: format!("PC {n}"),
        public_key: key_to_b64(&keys.public),
        user_sid: format!("S-1-{n}"),
        user_name: format!("user{n}"),
        profiles: vec![],
        last_seen_ms: 0,
    };
    store.add_pc(rec.clone()).unwrap();
    (rec, keys)
}

/// Waits (up to 10 s of real time) for `cond`.
async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(std::time::Instant::now() < deadline, "never happened: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A TCP connection to the hub that has sent its connection-kind byte.
async fn dial(addr: SocketAddr, kind: u8) -> TcpStream {
    let mut io = TcpStream::connect(addr).await.unwrap();
    io.set_nodelay(true).unwrap();
    io.write_all(&[kind]).await.unwrap();
    io
}

/// Reads until the hub closes the connection (a reset counts); the number of bytes it wrote.
async fn until_closed(io: &mut TcpStream) -> usize {
    let mut n = 0;
    let mut buf = [0u8; 1024];
    loop {
        match io.read(&mut buf).await {
            Ok(0) | Err(_) => return n,
            Ok(k) => n += k,
        }
    }
}

/// The PC end of a connection, relayed through a duplex stream: counts the bytes the hub wrote and can
/// dribble the PC's bytes one every 60 s.
struct Wire {
    slow: Arc<AtomicBool>,
    received: Arc<AtomicUsize>,
    /// Ends when the hub closed the connection.
    down: tokio::task::JoinHandle<()>,
}

fn pump(tcp: TcpStream) -> (DuplexStream, Wire) {
    let (mine, theirs) = tokio::io::duplex(1 << 16);
    let (mut tr, mut tw) = tcp.into_split();
    let (mut dr, mut dw) = tokio::io::split(theirs);
    let slow = Arc::new(AtomicBool::new(false));
    let received = Arc::new(AtomicUsize::new(0));
    let s = slow.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = match dr.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            if s.load(Ordering::SeqCst) {
                for b in &buf[..n] {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    if tw.write_all(&[*b]).await.is_err() {
                        return;
                    }
                }
            } else if tw.write_all(&buf[..n]).await.is_err() {
                return;
            }
        }
    });
    let r = received.clone();
    let down = tokio::spawn(async move {
        let mut buf = vec![0u8; 1 << 16];
        loop {
            match tr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    r.fetch_add(n, Ordering::SeqCst);
                    if dw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = dw.shutdown().await;
    });
    (mine, Wire { slow, received, down })
}

type Rd = SecureReader<ReadHalf<DuplexStream>>;
type Wr = SecureWriter<WriteHalf<DuplexStream>>;

/// An authenticated session as `keys`, hello exchanged.
async fn open(e: &Env, pc: &(PcRecord, Keys)) -> (Rd, Wr, Wire) {
    let (io, wire) = pump(dial(e.addr, SESSION).await);
    let (mut rd, mut wr) = noise::connect(io, &pc.1, &e.hub_public()).await.unwrap();
    let hello = Msg::Hello { protocol: SYNC_PROTOCOL, host_id: pc.0.host_id, name: "PC".into(), role: Role::Pc };
    wr.send(&Frame::new(&hello, vec![])).await.unwrap();
    let got: Msg = rd.recv().await.unwrap().msg().unwrap();
    assert!(matches!(got, Msg::Hello { role: Role::Hub, .. }), "{got:?}");
    (rd, wr, wire)
}

fn guest_info(keys: &Keys) -> PairInfo {
    PairInfo {
        host_id: Uuid::from_u128(5),
        name: "Laptop".into(),
        public_key: key_to_b64(&keys.public),
        user_sid: "S-1-5-21-5".into(),
        members: vec![],
        port: 47321,
        role: Role::Pc,
        addrs: vec![],
        user_name: "calin".into(),
    }
}

#[tokio::test]
async fn unknown_key_closed_no_bytes() {
    let e = env().await;
    let (io, wire) = pump(dial(e.addr, SESSION).await);
    assert!(noise::connect(io, &Keys::generate(), &e.hub_public()).await.is_err());
    wire.down.await.unwrap();
    assert_eq!(wire.received.load(Ordering::SeqCst), 0, "the hub wrote to a stranger");
    eventually("refusal counted", || e.hub.counters().snapshot().refused_handshakes == 1).await;
}

#[tokio::test]
async fn oversized_handshake_closed() {
    let e = env().await;
    let mut io = dial(e.addr, SESSION).await;
    let mut first = 257u16.to_le_bytes().to_vec();
    first.extend([7u8; 257]);
    let _ = io.write_all(&first).await;
    assert_eq!(until_closed(&mut io).await, 0);
    eventually("refusal counted", || e.hub.counters().snapshot().refused_handshakes == 1).await;
}

#[tokio::test(start_paused = true)]
async fn slow_handshake_closed_at_5s() {
    let e = env().await;
    let mut io = dial(e.addr, SESSION).await;
    let t = Instant::now();
    // the length prefix of a first message, then its first byte 4 s later: the 5 s are for the whole phase
    io.write_all(&96u16.to_le_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    io.write_all(&[1]).await.unwrap();
    assert_eq!(until_closed(&mut io).await, 0);
    let took = t.elapsed();
    assert!(took >= Duration::from_millis(4900) && took < Duration::from_millis(5500), "closed after {took:?}");
}

#[tokio::test]
async fn seventeenth_unauth_connection_refused() {
    let e = env().await;
    let mut silent = Vec::new();
    for _ in 0..16 {
        silent.push(TcpStream::connect(e.addr).await.unwrap());
    }
    eventually("16 connections waiting", || e.hub.unauthenticated() == 16).await;
    let mut extra = TcpStream::connect(e.addr).await.unwrap();
    let t = std::time::Instant::now();
    assert_eq!(until_closed(&mut extra).await, 0);
    assert!(t.elapsed() < Duration::from_secs(2), "not closed at once");
    eventually("refusal counted", || e.hub.counters().snapshot().refused_handshakes == 1).await;

    // once they are gone a linked PC gets in
    drop(silent);
    eventually("permits back", || e.hub.unauthenticated() == 0).await;
    let pc = linked(&e.store, 1);
    open(&e, &pc).await;
}

#[tokio::test]
async fn handshake_rate_limited() {
    let e = env().await;
    for _ in 0..60 {
        let mut io = dial(e.addr, b'X').await;
        until_closed(&mut io).await;
    }
    // the 61st within the minute is closed before the handshake, even for a linked PC
    let pc = linked(&e.store, 1);
    let (io, wire) = pump(dial(e.addr, SESSION).await);
    assert!(noise::connect(io, &pc.1, &e.hub_public()).await.is_err());
    wire.down.await.unwrap();
    assert_eq!(wire.received.load(Ordering::SeqCst), 0);
    eventually("61 refusals", || e.hub.counters().snapshot().refused_handshakes == 61).await;
}

#[test]
fn rate_window_slides() {
    let t0 = Instant::now();
    let mut w = RateWindow::default();
    for i in 0..60 {
        assert!(w.hit(t0 + Duration::from_millis(i * 100)), "attempt {i}");
    }
    assert!(!w.hit(t0 + Duration::from_secs(30)));
    // the first attempt leaves the window a minute after it was made, making room for one
    assert!(!w.hit(t0 + Duration::from_millis(59_999)));
    assert!(w.hit(t0 + Duration::from_millis(60_000)));
    assert!(!w.hit(t0 + Duration::from_millis(60_050)));
    assert!(w.hit(t0 + Duration::from_millis(60_100)));
}

#[tokio::test]
async fn pairing_without_window_closed() {
    let e = env().await;
    assert!(!e.hub.pairing_open());
    let mut io = dial(e.addr, PAIRING).await;
    assert_eq!(until_closed(&mut io).await, 0, "the hub answered a pairing request with no window open");
    eventually("failure counted", || e.hub.counters().snapshot().pairing_failures == 1).await;
}

#[tokio::test]
async fn three_wrong_codes_close_window() {
    let e = env().await;
    let code = e.hub.open_pairing(now_ms());
    assert_eq!(code.code.len(), HUB_CODE_DIGITS);
    let wrong = if code.code == "00000000" { "11111111" } else { "00000000" };
    let keys = Keys::generate();
    for i in 0..3 {
        assert!(e.hub.pairing_open(), "closed before attempt {i}");
        let mut io = dial(e.addr, PAIRING).await;
        assert_eq!(io.read_u8().await.unwrap(), STATUS_OK);
        let r = pair_guest(&mut io, wrong, &guest_info(&keys)).await;
        assert!(matches!(r, Err(seek_sync::Error::WrongCode)), "{r:?}");
    }
    assert!(!e.hub.pairing_open());
    // the right code no longer helps
    let mut io = dial(e.addr, PAIRING).await;
    assert_eq!(until_closed(&mut io).await, 0);
    assert!(e.store.pcs().is_empty());
    eventually("failures counted", || e.hub.counters().snapshot().pairing_failures == 4).await;
    eventually("outcome", || e.hub.pairing_outcome() == Some(PairOutcome::TooManyFailures)).await;
}

#[tokio::test]
async fn pairing_success_adds_pc_and_closes() {
    let e = env().await;
    let code = e.hub.open_pairing(now_ms());
    let keys = Keys::generate();
    let mine = guest_info(&keys);
    let mut io = dial(e.addr, PAIRING).await;
    assert_eq!(io.read_u8().await.unwrap(), STATUS_OK);
    let hub = pair_guest(&mut io, &code.code, &mine).await.unwrap();
    assert_eq!(hub.role, Role::Hub);
    assert_eq!(hub.host_id, e.store.hub_id());
    assert_eq!(hub.name, "Test Hub");
    assert_eq!(hub.public_key, key_to_b64(&e.hub_public()));
    assert_eq!((hub.user_sid.as_str(), hub.members.len(), hub.port), ("", 0, 47321));

    eventually("PC stored", || e.store.pc_by_key(&keys.public).is_some()).await;
    let rec = e.store.pc_by_key(&keys.public).unwrap();
    assert_eq!(rec.host_id, mine.host_id);
    assert_eq!((rec.name.as_str(), rec.user_sid.as_str(), rec.user_name.as_str()), ("Laptop", "S-1-5-21-5", "calin"));
    eventually("window closed", || !e.hub.pairing_open()).await;
    eventually("outcome", || e.hub.pairing_outcome() == Some(PairOutcome::Linked { name: "Laptop".into() })).await;
    assert_eq!(e.hub.pairing_state().window, 1);

    // the new PC is let in
    open(&e, &(rec, keys)).await;
}

#[tokio::test]
async fn a_paired_pc_is_accepted_the_instant_pairing_returns() {
    let e = env().await;
    let code = e.hub.open_pairing(now_ms());
    let keys = Keys::generate();
    let mine = guest_info(&keys);
    let mut io = dial(e.addr, PAIRING).await;
    assert_eq!(io.read_u8().await.unwrap(), STATUS_OK);
    pair_guest(&mut io, &code.code, &mine).await.unwrap();
    // no waiting: the hub saved the PC before it sent its reply
    assert!(e.store.pc_by_key(&keys.public).is_some());
    assert!(!e.hub.pairing_open());
    let rec = e.store.pc_by_key(&keys.public).unwrap();
    open(&e, &(rec, keys)).await;
}

#[tokio::test]
async fn tombstoned_key_gets_unlinked() {
    let e = env().await;
    let pc = linked(&e.store, 1);
    e.store.unlink_pc(pc.0.host_id).unwrap();
    let (io, wire) = pump(dial(e.addr, SESSION).await);
    let (mut rd, _wr) = noise::connect(io, &pc.1, &e.hub_public()).await.unwrap();
    assert_eq!(rd.recv().await.unwrap().msg::<Msg>().unwrap(), Msg::Unlinked);
    // then the hub closes the connection: nothing else
    match rd.recv().await {
        Err(seek_sync::Error::Io(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {}
        other => panic!("expected the connection to close, got {other:?}"),
    }
    wire.down.await.unwrap();
}

#[tokio::test]
async fn new_session_replaces_old() {
    let e = env().await;
    let pc = linked(&e.store, 1);
    let (mut rd1, _wr1, w1) = open(&e, &pc).await;
    let (mut rd2, mut wr2, _w2) = open(&e, &pc).await;
    tokio::time::timeout(Duration::from_secs(10), w1.down).await.expect("old session not closed").unwrap();
    assert!(rd1.recv().await.is_err());
    // the new one is served
    wr2.send(&Frame::new(&Msg::Pull { cursors: vec![] }, vec![])).await.unwrap();
    assert!(matches!(rd2.recv().await.unwrap().msg::<Msg>().unwrap(), Msg::Catalog { .. }));
    assert_eq!(rd2.recv().await.unwrap().msg::<Msg>().unwrap(), Msg::PullDone);
}

// The paused-clock session tests below pause only after the handshake and hello, so no real loopback I/O is
// in flight against the 5 s pre-auth timer. Afterwards auto-advance can only jump to the next timer: the
// PC's own 60 s sleeps or the hub's 3 min deadline. A byte that is still in flight when the clock jumps
// arrives at most one 60 s step late, far inside the 3 min margin, so the outcome cannot flip.
#[tokio::test]
async fn slow_frame_is_not_idle() {
    let e = env().await;
    let pc = linked(&e.store, 1);
    let (mut rd, mut wr, wire) = open(&e, &pc).await;
    tokio::time::pause();
    wire.slow.store(true, Ordering::SeqCst);
    let t = Instant::now();
    wr.send(&Frame::new(&Msg::Pull { cursors: vec![] }, vec![])).await.unwrap();
    assert!(matches!(rd.recv().await.unwrap().msg::<Msg>().unwrap(), Msg::Catalog { .. }));
    assert_eq!(rd.recv().await.unwrap().msg::<Msg>().unwrap(), Msg::PullDone);
    assert!(t.elapsed() >= Duration::from_secs(10 * 60), "the frame took only {:?}", t.elapsed());
}

#[tokio::test]
async fn silent_session_closed_after_3min() {
    let e = env().await;
    let pc = linked(&e.store, 1);
    let (_rd, _wr, wire) = open(&e, &pc).await;
    tokio::time::pause();
    let t = Instant::now();
    tokio::time::sleep(Duration::from_secs(179)).await;
    assert!(!wire.down.is_finished(), "closed before 3 min");
    wire.down.await.unwrap();
    let took = t.elapsed();
    assert!(took >= Duration::from_secs(179) && took <= Duration::from_secs(181), "closed after {took:?}");
    assert_eq!(e.hub.counters().snapshot().refused_handshakes, 0);
}

#[tokio::test]
async fn stalled_write_closed_after_3min() {
    let e = env().await;
    let pc = linked(&e.store, 1);
    // another PC's drive with a copy far larger than every buffer between the hub and the PC
    let b = linked(&e.store, 2);
    let bd = Uuid::from_u128(20);
    let drive = crate::session::tests::device(bd, b.0.host_id);
    e.store.set_pc_devices(b.0.host_id, vec![drive], vec![]).unwrap();
    let cursor = seek_sync::Cursor { generation: Uuid::from_u128(77), seq: 0 };
    e.store.put_full(b.0.host_id, bd, cursor, crate::store::fake_copy(&vec![9u8; 16 << 20])).unwrap();

    let (_rd, mut wr, wire) = open(&e, &pc).await;
    // Pull, then never read: the hub's writes stall once the buffers are full
    wr.send(&Frame::new(&Msg::Pull { cursors: vec![] }, vec![])).await.unwrap();
    // stalled = the copy itself is flowing (more than the relay's 64 KB buffer arrived; the catalog comes
    // first, while the hub is still reading the copy from disk) and then nothing more arrives
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut last = 0;
    loop {
        assert!(std::time::Instant::now() < deadline, "the hub's writes never stalled");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let now = wire.received.load(Ordering::SeqCst);
        if now > 1 << 16 && now == last {
            break;
        }
        last = now;
    }
    assert!(last < 16 << 20, "the whole copy got through ({last} bytes)");
    assert!(e.hub.has_session(pc.0.host_id));

    // Same margin as above: no I/O is in flight once the writes have stalled. The paused stretch is two jumps
    // of the clock, no polling: real time passing while paused would let the OS take a late burst of bytes
    // (real progress) and restart the hub's 3 min.
    tokio::time::pause();
    tokio::time::sleep(Duration::from_secs(170)).await;
    assert!(e.hub.has_session(pc.0.host_id), "closed before 3 min");
    tokio::time::sleep(Duration::from_secs(11)).await;
    assert!(!e.hub.has_session(pc.0.host_id), "still open after 3 min of stalled writes");
}
